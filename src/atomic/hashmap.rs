use crate::atomic::AtomicVec;
use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::borrow::Borrow;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Number of shards (buckets)
const NUM_SHARDS: usize = 4;
/// Default size of each shard's vector
const DEFAULT_SHARD_SIZE: usize = 128;
/// Load factor threshold for resizing
const LOAD_FACTOR_THRESHOLD: f64 = 0.75;

/// Entry in the hashmap - stored in collision chains
struct Entry<K, V> {
    key: K,
    value: ManuallyDrop<V>,
    next: AtomicPtr<Entry<K, V>>,
}

impl<K, V> Entry<K, V> {
    fn new(key: K, value: V) -> *mut Entry<K, V> {
        Box::into_raw(Box::new(Entry {
            key,
            value: ManuallyDrop::new(value),
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

/// A slot in the shard's vector
struct Slot<K, V> {
    /// Head of the collision chain for this slot
    head: AtomicPtr<Entry<K, V>>,
    /// Mutex for this specific slot
    mutex: CachePadded<Mutex>,
}

impl<K, V> Slot<K, V> {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
            mutex: CachePadded::new(Mutex::new()),
        }
    }
}

/// A shard containing a vector of slots
struct Shard<K, V> {
    /// AtomicVec storing pointers to slots
    slots: AtomicVec<*mut Slot<K, V>>,
    /// Current size of this shard
    size: AtomicUsize,
    mutex: Mutex,
}

impl<K: Eq + Hash, V> Shard<K, V> {
    fn new(size: usize) -> Self {
        Self {
            slots: AtomicVec::init_with(size, || Box::into_raw(Box::new(Slot::new()))),
            size: AtomicUsize::new(size),
            mutex: Mutex::new(),
        }
    }

    #[inline]
    fn get_slot(&self, index: usize) -> Option<&Slot<K, V>> {
        Some(unsafe { &**self.slots.get(index)? })
    }

    fn insert(&self, slot_idx: usize, key: K, value: V) -> Option<V> {
        self.mutex.lock_shared();

        let slot = self.get_slot(slot_idx).expect("invalid slot index");

        slot.mutex.lock_exclusive();

        // check collision chain
        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key == key {
                    let old_value = ManuallyDrop::into_inner(std::ptr::read(&(*cur).value));
                    (*cur).value = ManuallyDrop::new(value);
                    slot.mutex.unlock_exclusive();
                    return Some(old_value);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        // insert new entry
        let new_entry = Entry::new(key, value);
        unsafe {
            (*new_entry)
                .next
                .store(slot.head.load(Ordering::Acquire), Ordering::Release);
        }
        slot.head.store(new_entry, Ordering::Release);

        slot.mutex.unlock_exclusive();
        self.mutex.unlock_shared();
        None
    }

    fn get<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.mutex.lock_shared();
        let slot = self.get_slot(slot_idx)?;
        slot.mutex.lock_shared();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    let guard = WatchGuardRef::new(&*(*cur).value, slot.mutex.deref().clone());
                    self.mutex.unlock_shared();
                    return Some(guard);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_shared();
        self.mutex.unlock_shared();
        None
    }

    fn get_mut<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.mutex.lock_shared();
        let slot = self.get_slot(slot_idx)?;
        slot.mutex.lock_exclusive();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    let guard = WatchGuardMut::new(&mut *(*cur).value, slot.mutex.deref().clone());
                    return Some(guard);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_exclusive();
        self.mutex.unlock_shared();
        None
    }

    fn remove<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.mutex.lock_shared();
        let slot = self.get_slot(slot_idx)?;
        slot.mutex.lock_exclusive();

        let mut cur = slot.head.load(Ordering::Acquire);
        let mut prev: *mut Entry<K, V> = null_mut();

        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    let next = (*cur).next.load(Ordering::Acquire);
                    if prev.is_null() {
                        slot.head.store(next, Ordering::Release);
                    } else {
                        (*prev).next.store(next, Ordering::Release);
                    }

                    let value = ManuallyDrop::into_inner(std::ptr::read(&(*cur).value));
                    drop(Box::from_raw(cur));

                    slot.mutex.unlock_exclusive();
                    return Some(value);
                }
                prev = cur;
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_exclusive();
        self.mutex.unlock_shared();
        None
    }

    fn contains<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.mutex.lock_shared();
        let slot = self.get_slot(slot_idx).unwrap();
        slot.mutex.lock_shared();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    slot.mutex.unlock_shared();
                    return true;
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_shared();
        self.mutex.unlock_shared();
        false
    }

    fn maybe_resize<S: BuildHasher>(&self, hasher: &S) -> bool {
        let old_size = self.size.load(Ordering::Acquire);
        let count = self.len();

        if (count as f64) < (old_size as f64 * LOAD_FACTOR_THRESHOLD) {
            return false;
        }

        let prev_values = self.slots.as_vec();

        let new_size = self.slots.reset_with(old_size * 2, || Box::into_raw(Box::new(Slot::new())));

        // rehash all entries
        for slot in prev_values {
            let old_slot = unsafe { &*slot };
            old_slot.mutex.lock_exclusive();

            let mut cur_entry = old_slot.head.load(Ordering::Acquire);
            while !cur_entry.is_null() {
                let entry = unsafe { &(*cur_entry) };
                let next_entry = entry.next.load(Ordering::Acquire);

                // recompute new slot index
                let mut h = hasher.build_hasher();
                entry.key.hash(&mut h);
                let new_slot_idx = (h.finish() as usize) % new_size;

                let new_slot = unsafe { &**self.slots.get(new_slot_idx).unwrap() };

                entry
                    .next
                    .store(new_slot.head.load(Ordering::Acquire), Ordering::Release);

                new_slot.head.store(cur_entry, Ordering::Release);

                cur_entry = next_entry
            }

            old_slot.mutex.unlock_exclusive();
        }

        self.size.store(new_size, Ordering::Release);
        true
    }

    fn len(&self) -> usize {
        let mut total = 0;
        let size = self.size.load(Ordering::Acquire);
        for i in 0..size {
            if let Some(slot_ptr) = self.slots.get(i) {
                unsafe {
                    let slot = &**slot_ptr;
                    let mut cur = slot.head.load(Ordering::Acquire);
                    while !cur.is_null() {
                        total += 1;
                        cur = (*cur).next.load(Ordering::Acquire);
                    }
                }
            }
        }
        total
    }
}

impl<K, V> Shard<K, V> {
    fn cleanup(&self) {
        let size = self.size.load(Ordering::Acquire);
        for i in 0..size {
            if let Some(slot_ptr) = self.slots.get(i) {
                unsafe {
                    let slot = &**slot_ptr;
                    let mut cur = slot.head.load(Ordering::Acquire);
                    while !cur.is_null() {
                        let next = (*cur).next.load(Ordering::Acquire);
                        ManuallyDrop::drop(&mut (*cur).value);
                        drop(Box::from_raw(cur));
                        cur = next;
                    }
                    drop(Box::from_raw(*slot_ptr));
                }
            }
        }
    }
}

/// Inner structure containing all shards and metadata
struct AtomicInner<K, V, S> {
    shards: Vec<Shard<K, V>>,
    len: CachePadded<AtomicUsize>,
    ref_count: CachePadded<AtomicUsize>,
    hasher: S,
    global_lock: CachePadded<Mutex>,
}

/// Thread-safe HashMap with sharded storage
#[repr(transparent)]
pub struct AtomicHashMap<K, V, S = RandomState> {
    ptr: *const AtomicInner<K, V, S>,
}

unsafe impl<K: Send, V: Send, S> Send for AtomicHashMap<K, V, S> {}
unsafe impl<K: Send, V: Send, S> Sync for AtomicHashMap<K, V, S> {}

impl<K, V, S> UnwindSafe for AtomicHashMap<K, V, S> {}
impl<K, V, S> RefUnwindSafe for AtomicHashMap<K, V, S> {}

impl<K: Eq + Hash, V> AtomicHashMap<K, V, RandomState> {
    pub fn new() -> Self {
        Self::with_shard_size(DEFAULT_SHARD_SIZE)
    }

    pub fn with_shard_size(shard_size: usize) -> Self {
        Self::with_hasher_and_shard_size(RandomState::default(), shard_size)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    pub fn with_hasher_and_shard_size(hasher: S, shard_size: usize) -> Self {
        let inner = Box::new(AtomicInner {
            shards: (0..NUM_SHARDS).map(|_| Shard::new(shard_size)).collect(),
            len: CachePadded::new(AtomicUsize::new(0)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
            hasher,
            global_lock: CachePadded::new(Mutex::new()),
        });

        Self {
            ptr: Box::into_raw(inner),
        }
    }

    fn hash<Q: ?Sized + Hash>(&self, key: &Q) -> u64 {
        let mut hasher = self.inner().hasher.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    #[inline]
    fn get_indices(&self, hash: u64) -> (usize, usize) {
        let shard_idx = (hash as usize) % NUM_SHARDS;
        let shard_size = self.inner().shards[shard_idx].size.load(Ordering::Acquire);
        let slot_idx = (hash as usize) % shard_size;
        (shard_idx, slot_idx)
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let hash = self.hash(&key);
        let (shard_idx, mut slot_idx) = self.get_indices(hash);
        let shard = &self.inner().shards[shard_idx];

        shard.mutex.lock_exclusive();
        if shard.maybe_resize(self.hasher()) {
            (_, slot_idx) = self.get_indices(hash);
        }
        shard.mutex.unlock_exclusive();

        let res = shard.insert(slot_idx, key, value);
        if res.is_none() {
            self.inner().len.fetch_add(1, Ordering::Relaxed);
        }
        res
    }

    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);
        self.inner().shards[shard_idx].get(slot_idx, key)
    }

    pub fn get_mut<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);
        self.inner().shards[shard_idx].get_mut(slot_idx, key)
    }

    pub fn remove<Q: ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);
        let res = self.inner().shards[shard_idx].remove(slot_idx, key);
        if res.is_some() {
            self.inner().len.fetch_sub(1, Ordering::Relaxed);
        }
        res
    }

    pub fn contains_key<Q: ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);
        self.inner().shards[shard_idx].contains(slot_idx, key)
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.inner().hasher
    }

    pub fn snapshot<KC, VC>(&self) -> Vec<(KC, VC)>
    where
        K: Clone + Into<KC>,
        V: Clone + Into<VC>,
    {
        let mut result = Vec::with_capacity(self.len());

        for shard in &self.inner().shards {
            let size = shard.size.load(Ordering::Acquire);
            for i in 0..size {
                if let Some(slot) = shard.get_slot(i) {
                    slot.mutex.lock_shared();

                    let mut cur = slot.head.load(Ordering::Acquire);
                    while !cur.is_null() {
                        unsafe {
                            let entry = &*cur;
                            result.push((entry.key.clone().into(), (*entry.value).clone().into()));
                            cur = entry.next.load(Ordering::Acquire);
                        }
                    }

                    slot.mutex.unlock_shared();
                }
            }
        }

        result
    }
}

impl<K, V, S> AtomicHashMap<K, V, S> {
    #[inline(always)]
    fn inner(&self) -> &AtomicInner<K, V, S> {
        unsafe { &*self.ptr }
    }

    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K, V, S> Clone for AtomicHashMap<K, V, S> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<K, V, S> Drop for AtomicHashMap<K, V, S> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            for shard in &self.inner().shards {
                shard.cleanup();
            }
            unsafe {
                drop(Box::from_raw(self.ptr as *mut AtomicInner<K, V, S>));
            }
        }
    }
}

impl<K, V, S> fmt::Debug for AtomicHashMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicHashMap")
            .field("shards", &NUM_SHARDS)
            .field("len", &self.len())
            .field("ref_count", &self.inner().ref_count.load(Ordering::Relaxed))
            .finish()
    }
}

impl<K: Eq + Hash, V> Default for AtomicHashMap<K, V, RandomState> {
    fn default() -> Self {
        Self::new()
    }
}
