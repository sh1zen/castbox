use crate::atomic::{AtomicArray, AtomicVec};
use crate::mutex::{Backoff, Mutex, WatchGuardMut, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::borrow::Borrow;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Number of shards (buckets)
const NUM_SHARDS: usize = 32;
/// Default size of each shard's vector
const DEFAULT_SHARD_SIZE: usize = 64;
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
    slots: AtomicArray<*mut Slot<K, V>>,
    /// Current size of this shard
    size: AtomicUsize,
    /// Accurate count of entries in this shard (updated atomically)
    count: CachePadded<AtomicUsize>,
    /// Mutex for structural operations (resize only)
    mutex: Mutex,
    /// Flag indicating resize is in progress
    resize_in_progress: AtomicBool,
}

impl<K: Eq + Hash, V> Shard<K, V> {
    fn new(size: usize) -> Self {
        Self {
            slots: AtomicArray::init_with(size, || Box::into_raw(Box::new(Slot::new()))),
            size: AtomicUsize::new(size),
            count: CachePadded::new(AtomicUsize::new(0)),
            mutex: Mutex::new(),
            resize_in_progress: AtomicBool::new(false),
        }
    }

    #[inline]
    fn get_slot(&self, index: usize) -> Option<&Slot<K, V>> {
        Some(unsafe { &**self.slots.get(index)? })
    }

    /// Insert without shard-level locking (caller must ensure no resize conflicts)
    fn insert(&self, slot_idx: usize, key: K, value: V) -> Option<V> {
        let slot = self.get_slot(slot_idx).expect("invalid slot index");

        slot.mutex.lock_exclusive();

        // check collision chain
        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key == key {
                    // Key exists, replace value (no count change)
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

        // Increment shard count for new entry
        self.count.fetch_add(1, Ordering::Relaxed);

        slot.mutex.unlock_exclusive();
        None
    }

    /// Get with only slot-level locking
    fn get<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        // Wait if resize is in progress
        if self.resize_in_progress.load(Ordering::Acquire) {
            let backoff = Backoff::new();
            while self.resize_in_progress.load(Ordering::Acquire) {
                backoff.snooze();
            }
        }

        let slot = self.get_slot(slot_idx)?;
        slot.mutex.lock_shared();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).key.borrow() == key {
                    let guard = WatchGuardRef::new(&*(*cur).value, slot.mutex.deref().clone());
                    return Some(guard);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_shared();
        None
    }

    /// Get mutable with only slot-level locking
    fn get_mut<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        // Wait if resize is in progress
        if self.resize_in_progress.load(Ordering::Acquire) {
            let backoff = Backoff::new();
            while self.resize_in_progress.load(Ordering::Acquire) {
                backoff.snooze();
            }
        }

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
        None
    }

    /// Remove with only slot-level locking
    fn remove<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        // Wait if resize is in progress
        if self.resize_in_progress.load(Ordering::Acquire) {
            let backoff = Backoff::new();
            while self.resize_in_progress.load(Ordering::Acquire) {
                backoff.snooze();
            }
        }

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

                    // Decrement shard count
                    self.count.fetch_sub(1, Ordering::Relaxed);

                    slot.mutex.unlock_exclusive();
                    return Some(value);
                }
                prev = cur;
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_exclusive();
        None
    }

    /// Contains key check with only slot-level locking
    fn contains<Q: ?Sized>(&self, slot_idx: usize, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        // Wait if resize is in progress
        if self.resize_in_progress.load(Ordering::Acquire) {
            let backoff = Backoff::new();
            while self.resize_in_progress.load(Ordering::Acquire) {
                backoff.snooze();
            }
        }

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
        false
    }

    /// Resize with full synchronization
    fn maybe_resize<S: BuildHasher>(&self, hasher: &S) -> bool {
        let old_size = self.size.load(Ordering::Acquire);
        let current_count = self.count.load(Ordering::Acquire);

        // Check load factor
        if (current_count as f64) < (old_size as f64 * LOAD_FACTOR_THRESHOLD) {
            return false;
        }

        // Try to set resize flag atomically (only one thread can resize)
        if self.resize_in_progress.compare_exchange(
            false,
            true,
            Ordering::Acquire,
            Ordering::Relaxed
        ).is_err() {
            // Another thread is already resizing
            return false;
        }

        // Acquire exclusive lock on shard structure
        self.mutex.lock_exclusive();

        // Lock all old slots exclusively to prevent concurrent access during rehash
        for i in 0..old_size {
            if let Some(slot) = self.get_slot(i) {
                slot.mutex.lock_exclusive();
            }
        }

        // Save old slot pointers before reset
        let old_slot_ptrs = self.slots.as_vec();
        let new_size = old_size * 2;

        // Reset with new size (this deallocates the old AtomicArray internal structure)
        let result = self.slots.reset_with(new_size, || Box::into_raw(Box::new(Slot::new())));

        if let Ok(actual_size) = result {
            // Count entries during rehash to verify count
            let mut actual_count = 0;

            // Rehash all entries from old slots to new slots
            for slot_ptr in &old_slot_ptrs {
                let old_slot = unsafe { &**slot_ptr };

                let mut cur_entry = old_slot.head.load(Ordering::Acquire);
                while !cur_entry.is_null() {
                    let entry = unsafe { &(*cur_entry) };
                    let next_entry = entry.next.load(Ordering::Acquire);

                    // Count this entry
                    actual_count += 1;

                    // Recompute new slot index
                    let mut h = hasher.build_hasher();
                    entry.key.hash(&mut h);
                    let new_slot_idx = (h.finish() as usize) % actual_size;

                    let new_slot = unsafe { &**self.slots.get(new_slot_idx).unwrap() };

                    // Insert into new chain (no need to lock, we're building)
                    entry
                        .next
                        .store(new_slot.head.load(Ordering::Acquire), Ordering::Release);
                    new_slot.head.store(cur_entry, Ordering::Release);

                    cur_entry = next_entry;
                }

                // Unlock the old slot before deallocating
                old_slot.mutex.unlock_exclusive();
            }

            // Now deallocate all old slots (they're empty now, entries moved to new slots)
            for slot_ptr in old_slot_ptrs {
                unsafe {
                    drop(Box::from_raw(slot_ptr));
                }
            }

            // Update size
            self.size.store(actual_size, Ordering::Release);

            // Correct the count if needed (handles any drift)
            self.count.store(actual_count, Ordering::Release);
        } else {
            // Resize failed, unlock all old slots
            for i in 0..old_size {
                if let Some(slot) = self.get_slot(i) {
                    slot.mutex.unlock_exclusive();
                }
            }
        }

        // Release shard lock
        self.mutex.unlock_exclusive();

        // Clear resize flag
        self.resize_in_progress.store(false, Ordering::Release);

        result.is_ok()
    }

    /// Get the current count (O(1) operation)
    #[inline]
    fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
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

        // Check if resize is needed (lock-free check using atomic count)
        if !shard.resize_in_progress.load(Ordering::Acquire) {
            let old_size = shard.size.load(Ordering::Acquire);
            let current_count = shard.count.load(Ordering::Acquire);

            if (current_count as f64) >= (old_size as f64 * LOAD_FACTOR_THRESHOLD) {
                // Attempt resize (internally synchronized)
                shard.maybe_resize(self.hasher());

                // Recalculate slot index after potential resize
                let shard_size = shard.size.load(Ordering::Acquire);
                slot_idx = (hash as usize) % shard_size;
            }
        } else {
            // Wait for resize to complete
            let backoff = Backoff::new();
            while shard.resize_in_progress.load(Ordering::Acquire) {
                backoff.snooze();
            }

            // Recalculate slot index
            let shard_size = shard.size.load(Ordering::Acquire);
            slot_idx = (hash as usize) % shard_size;
        }

        // Perform insert (only slot-level locking)
        let res = shard.insert(slot_idx, key, value);
        if res.is_none() {
            // New entry was added, increment global count
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
            // Entry was removed, decrement global count
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
            // Wait if resize in progress
            if shard.resize_in_progress.load(Ordering::Acquire) {
                let backoff = Backoff::new();
                while shard.resize_in_progress.load(Ordering::Acquire) {
                    backoff.snooze();
                }
            }

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

    /// Returns the total number of elements in the map (O(1) operation)
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