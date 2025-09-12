use crate::mutex::{Backoff, Mutex, WatchGuardMut, WatchGuardRef};
use std::borrow::Borrow;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::{self, null_mut};
use std::sync::atomic;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

const BUCKET_AVAILABLE: bool = true;
const BUCKET_UPDATING: bool = false;
const DEFAULT_BUCKETS: usize = 64;

const LOAD_FACTOR: f64 = 0.75;

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

struct AtomicInner<K, V, S> {
    buckets: Vec<Bucket<K, V>>,
    lock: Mutex,
    len: AtomicUsize,
    ref_count: AtomicUsize,
    hasher: S,
}

#[repr(transparent)]
pub struct AtomicHashMap<K, V, S = RandomState> {
    ptr: *const AtomicInner<K, V, S>,
}

unsafe impl<K: Send, V: Send, S> Send for AtomicHashMap<K, V, S> {}
unsafe impl<K: Send, V: Send, S> Sync for AtomicHashMap<K, V, S> {}

impl<K, V, S> UnwindSafe for AtomicHashMap<K, V, S> {}
impl<K, V, S> RefUnwindSafe for AtomicHashMap<K, V, S> {}

impl<K: Eq + Hash, V> AtomicHashMap<K, V, RandomState> {
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
            hasher: RandomState::default(),
        }));
        Self { ptr }
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    #[inline(always)]
    fn inner(&self) -> &AtomicInner<K, V, S> {
        unsafe { &*self.ptr }
    }

    pub fn hasher(&self) -> &S {
        &self.inner().hasher
    }

    fn hash<Q: ?Sized + Hash>(&self, key: &Q) -> u64 {
        let mut hasher = self.inner().hasher.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    pub fn insert(&self, key: K, value: V) {
        let bucket = self.find_bucket(&key).unwrap();

        self.inner().lock.lock_group();
        bucket.lock();

        let head = bucket.head.load(Ordering::Acquire);
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

        let new_item = Item::new(key, value);
        unsafe { (*new_item).next.store(head, Ordering::Release) };
        bucket.head.store(new_item, Ordering::Release);
        bucket.release();

        self.inner().len.fetch_add(1, Ordering::Relaxed);
        self.inner().lock.unlock_group();

        // qui controlliamo se ridimensionare
        self.maybe_resize();
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
        let h = self.hash(key);
        let bucket_idx = h as usize % self.inner().buckets.len();
        let bucket = &self.inner().buckets[bucket_idx];

        Some(bucket)
    }

    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    fn maybe_resize(&self) {
        let inner = self.inner();
        let len = inner.len.load(Ordering::Acquire);
        let cap = inner.buckets.len();

        if (len as f64) >= (cap as f64 * LOAD_FACTOR) {
            self.resize();
        }
    }

    fn resize(&self) {
        let inner = self.inner();

        // blocco esclusivo globale
        inner.lock.lock_exclusive();

        let old_buckets = &inner.buckets;
        let new_cap = old_buckets.len() * 2;
        let new_buckets: Vec<Bucket<K, V>> = (0..new_cap).map(|_| Bucket::new()).collect();

        // reinserimento degli item
        for bucket in old_buckets {
            let mut cur = bucket.head.load(Ordering::Acquire);
            while !cur.is_null() {
                unsafe {
                    let item = &*cur;
                    let h = {
                        let mut hasher = inner.hasher.build_hasher();
                        item.key.hash(&mut hasher);
                        hasher.finish() as usize % new_cap
                    };

                    let new_bucket = &new_buckets[h];
                    let next = item.next.load(Ordering::Acquire);

                    // prepend
                    item.next
                        .store(new_bucket.head.load(Ordering::Acquire), Ordering::Release);
                    new_bucket.head.store(cur, Ordering::Release);

                    cur = next;
                }
            }
        }

        // sostituiamo i bucket
        let inner_mut = unsafe { &mut *(self.ptr as *mut AtomicInner<K, V, S>) };
        inner_mut.buckets = new_buckets;

        inner.lock.unlock_exclusive();
    }
}

impl<K, V, S> Clone for AtomicHashMap<K, V, S> {
    fn clone(&self) -> Self {
        let inner = unsafe { &*self.ptr };
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<K, V, S> Drop for AtomicHashMap<K, V, S> {
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

            unsafe { drop(Box::from_raw(self.ptr as *mut AtomicInner<K, V, S>)) };
        }
    }
}

impl<K, V, S> fmt::Debug for AtomicHashMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = unsafe { &*self.ptr };
        f.debug_struct("AtomicHashMap")
            .field("len", &inner.len)
            .field("buckets", &inner.buckets.len())
            .finish()
    }
}

pub struct Iter<'a, K, V, S> {
    map: &'a AtomicHashMap<K, V, S>,
    bucket_idx: usize,
    current: *mut Item<K, V>,
}

impl<'a, K: Eq + Hash, V, S: BuildHasher + Clone> Iter<'a, K, V, S> {
    fn new(map: &'a AtomicHashMap<K, V, S>) -> Self {
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

impl<'a, K: Eq + Hash, V, S: BuildHasher + Clone> Iterator for Iter<'a, K, V, S> {
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

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    pub fn iter(&self) -> Iter<'_, K, V, S> {
        self.inner().lock.lock_exclusive();
        Iter::new(self)
    }
}

impl<'a, K, V, S> Drop for Iter<'a, K, V, S> {
    fn drop(&mut self) {
        unsafe {
            (&*self.map.ptr).lock.unlock_exclusive();
        }
    }
}
