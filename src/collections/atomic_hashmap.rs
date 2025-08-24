use crate::mutex::{Backoff, Mutex, WatchGuardMut, WatchGuardRef};
use std::borrow::Borrow;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::{self, null_mut};
use std::sync::atomic;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

const BUCKET_AVAILABLE: bool = true;
const BUCKET_UPDATING: bool = false;
const DEFAULT_BUCKETS: usize = 256;

struct Item<K, V> {
    key: K,
    value: ManuallyDrop<V>,
    next: AtomicPtr<Item<K, V>>,
}

impl<K, V> Item<K, V> {
    fn new(key: K, value: V) -> *mut Item<K, V> {
        Box::into_raw(Box::new(Item {
            key,
            value: ManuallyDrop::new(value),
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

struct Bucket<K, V> {
    head: AtomicPtr<Item<K, V>>,
    ref_locked: Mutex,
    state: AtomicBool,
}

impl<K, V> Bucket<K, V> {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
            state: AtomicBool::new(BUCKET_AVAILABLE),
            ref_locked: Mutex::new(),
        }
    }

    #[inline]
    fn lock(&self) {
        let backoff = Backoff::new();
        while self
            .state
            .compare_exchange(
                BUCKET_AVAILABLE,
                BUCKET_UPDATING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            backoff.snooze();
        }
    }

    #[inline]
    fn release(&self) {
        self.state.store(BUCKET_AVAILABLE, Ordering::Release);
    }
}

struct AtomicInner<K, V> {
    buckets: Vec<Bucket<K, V>>,
    lock: Mutex,
    len: AtomicUsize,
    ref_count: AtomicUsize,
}

#[repr(transparent)]
pub struct AtomicHashMap<K, V> {
    ptr: *const AtomicInner<K, V>,
}

unsafe impl<K: Send, V: Send> Send for AtomicHashMap<K, V> {}
unsafe impl<K: Send, V: Send> Sync for AtomicHashMap<K, V> {}

impl<K, V> UnwindSafe for AtomicHashMap<K, V> {}
impl<K, V> RefUnwindSafe for AtomicHashMap<K, V> {}

impl<K: Eq + Hash, V> AtomicHashMap<K, V> {
    /// Create a new AtomicHashMap with default buckets size
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUCKETS)
    }

    /// Create a new AtomicHashMap with specified buckets size
    pub fn with_capacity(bucket_count: usize) -> Self {
        let buckets = (0..bucket_count).map(|_| Bucket::new()).collect();
        let ptr = Box::into_raw(Box::new(AtomicInner {
            buckets,
            len: AtomicUsize::new(0),
            ref_count: AtomicUsize::new(1),
            lock: Mutex::new(),
        }));
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicInner<K, V> {
        unsafe { &*self.ptr }
    }

    fn hash<Q: ?Sized + Hash>(key: &Q) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    pub fn insert(&self, key: K, value: V) {
        let bucket = self.find_bucket(&key).unwrap();

        // handle iter locking
        self.inner().lock.lock_group();
        // lock current bucket
        bucket.lock();

        let head = bucket.head.load(Ordering::Acquire);

        // check if key exists → update
        let mut cur = head;
        while !cur.is_null() {
            unsafe {
                if (*cur).key == key {
                    bucket.ref_locked.lock_exclusive();
                    ManuallyDrop::drop(&mut (*cur).value);
                    bucket.ref_locked.unlock_exclusive();
                    (*cur).value = ManuallyDrop::new(value);
                    bucket.release();
                    self.inner().lock.unlock_group();
                    return;
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        // prepend new
        let new_item = Item::new(key, value);
        unsafe { (*new_item).next.store(head, Ordering::Release) };
        bucket.head.store(new_item, Ordering::Release);
        bucket.release();

        self.inner().len.fetch_add(1, Ordering::Relaxed);
        self.inner().lock.unlock_group();
    }

    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let bucket = self.find_bucket(key)?;

        // handle iter locking
        self.inner().lock.lock_group();
        bucket.lock();

        let mut cur = bucket.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    bucket.ref_locked.lock_group();
                    let w_ref = WatchGuardRef::new(&*(*cur).value, bucket.ref_locked.clone());
                    bucket.release();
                    self.inner().lock.unlock_group();
                    return Some(w_ref);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        bucket.release();
        self.inner().lock.unlock_group();
        None
    }

    pub fn get_mut<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let bucket = self.find_bucket(key)?;

        // handle iter locking
        self.inner().lock.lock_group();
        // lock current bucket
        bucket.lock();

        let mut cur = bucket.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    bucket.ref_locked.lock_exclusive();
                    let w_ref = WatchGuardMut::new(&mut *(*cur).value, bucket.ref_locked.clone());

                    bucket.release();
                    self.inner().lock.unlock_group();
                    return Some(w_ref);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        bucket.release();
        self.inner().lock.unlock_group();
        None
    }

    pub fn remove<Q: ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let bucket = self.find_bucket(key)?;

        // handle iter locking
        self.inner().lock.lock_group();
        // lock current bucket
        bucket.lock();

        let mut cur = bucket.head.load(Ordering::Acquire);
        let mut prev: *mut Item<K, V> = null_mut();

        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    let next = (*cur).next.load(Ordering::Acquire);
                    if prev.is_null() {
                        bucket.head.store(next, Ordering::Release);
                    } else {
                        (*prev).next.store(next, Ordering::Release);
                    }

                    self.inner().len.fetch_sub(1, Ordering::Relaxed);

                    bucket.release();

                    bucket.ref_locked.lock_exclusive();
                    let val = ManuallyDrop::into_inner(ptr::read(&(*cur).value));
                    drop(Box::from_raw(cur));
                    bucket.ref_locked.unlock_exclusive();
                    self.inner().lock.unlock_group();
                    return Some(val);
                }
                prev = cur;
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        bucket.release();
        self.inner().lock.unlock_group();
        None
    }

    #[inline]
    fn find_bucket<Q: ?Sized>(&self, key: &Q) -> Option<&Bucket<K, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let h = Self::hash(key);
        let bucket_idx = h as usize % self.inner().buckets.len();
        let bucket = &self.inner().buckets[bucket_idx];

        Some(bucket)
    }

    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }
}

impl<K, V> Clone for AtomicHashMap<K, V> {
    fn clone(&self) -> Self {
        let inner = unsafe { &*self.ptr };
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<K, V> Drop for AtomicHashMap<K, V> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);

            for bucket in &inner.buckets {
                let mut cur = bucket.head.load(Ordering::Acquire);
                while !cur.is_null() {
                    unsafe {
                        let mut boxed = Box::from_raw(cur);
                        ManuallyDrop::drop(&mut boxed.value);
                        cur = boxed.next.load(Ordering::Acquire);
                    }
                }
            }

            unsafe { drop(Box::from_raw(self.ptr as *mut AtomicInner<K, V>)) };
        }
    }
}

impl<K, V> fmt::Debug for AtomicHashMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = unsafe { &*self.ptr };
        f.debug_struct("AtomicHashMap")
            .field("len", &inner.len)
            .field("buckets", &inner.buckets.len())
            .finish()
    }
}

pub struct Iter<'a, K, V> {
    map: &'a AtomicHashMap<K, V>,
    bucket_idx: usize,
    current: *mut Item<K, V>,
}

impl<'a, K: Eq + Hash, V> Iter<'a, K, V> {
    fn new(map: &'a AtomicHashMap<K, V>) -> Self {
        let mut it = Iter {
            map,
            bucket_idx: 0,
            current: null_mut(),
        };
        // first valid bucket
        it.advance_bucket();
        it
    }

    fn advance_bucket(&mut self) {
        while self.bucket_idx < self.map.inner().buckets.len() && self.current.is_null() {
            let bucket = &self.map.inner().buckets[self.bucket_idx];
            self.current = bucket.head.load(Ordering::Acquire);
            if self.current.is_null() {
                self.bucket_idx += 1;
            }
        }
    }
}

impl<'a, K: Eq + Hash, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.bucket_idx >= self.map.inner().buckets.len() {
            return None;
        }

        let cur_ptr = self.current;
        if cur_ptr.is_null() {
            return None;
        }

        let bucket = &self.map.inner().buckets[self.bucket_idx];
        let backoff = Backoff::new();

        while bucket.ref_locked.is_locked_exclusive() {
            backoff.snooze();
        }

        let item = unsafe { &*cur_ptr };
        // move to next
        self.current = item.next.load(Ordering::Acquire);
        if self.current.is_null() {
            self.bucket_idx += 1;
            self.advance_bucket();
        }
        Some((&item.key, &*item.value))
    }
}

impl<K: Eq + Hash, V> AtomicHashMap<K, V> {
    pub fn iter(&self) -> Iter<'_, K, V> {
        self.inner().lock.lock_exclusive();
        Iter::new(self)
    }
}

impl<'a, K, V> Drop for Iter<'a, K, V> {
    fn drop(&mut self) {
        unsafe {
            (&*self.map.ptr).lock.unlock_exclusive();
        }
    }
}
