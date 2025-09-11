use crate::core::SpinLockCell;
use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Default number of shards (power of 2)
const DEFAULT_SHARD_AMOUNT: usize = 64;

/// Entry in the hashmap - stored in collision chains
struct Entry<K, V> {
    key: K,
    value: ManuallyDrop<V>,
    hash: u64,
    next: AtomicPtr<Entry<K, V>>,
}

impl<K, V> Entry<K, V> {
    fn new(key: K, value: V, hash: u64) -> *mut Entry<K, V> {
        Box::into_raw(Box::new(Entry {
            key,
            value: ManuallyDrop::new(value),
            hash,
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

/// A slot in the bucket array
struct Slot<K, V> {
    head: AtomicPtr<Entry<K, V>>,
    mutex: Mutex,
}

impl<K, V> Slot<K, V> {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
            mutex: Mutex::new(),
        }
    }
}

/// A shard containing slots
struct Shard<K, V> {
    /// Each slot is individually padded to avoid false sharing between slots.
    slots: Vec<CachePadded<Slot<K, V>>>,
    capacity: usize,
    /// Per-shard count padded to avoid bouncing between cores.
    count: CachePadded<AtomicUsize>,
}

impl<K: Eq + Hash, V> Shard<K, V> {
    fn new(capacity: usize) -> Self {
        let mut slots: Vec<CachePadded<Slot<K, V>>> = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(CachePadded::new(Slot::new()));
        }

        Self {
            slots,
            capacity,
            count: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    #[inline]
    fn get_slot(&self, hash: u64) -> &Slot<K, V> {
        let idx = (hash as usize) % self.capacity;
        // indexing yields &CachePadded<Slot<...>>; deref to &Slot via Deref
        &*self.slots[idx]
    }

    fn insert(&self, key: K, value: V, hash: u64) -> Option<V> {
        // acquire shard read-phase to allow many concurrent ops on different shards

        let slot = self.get_slot(hash);
        slot.mutex.lock_exclusive();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).hash == hash && (*cur).key == key {
                    // replace existing value
                    let old_value = ManuallyDrop::into_inner(std::ptr::read(&(*cur).value));
                    (*cur).value = ManuallyDrop::new(value);
                    slot.mutex.unlock_exclusive();
                    return Some(old_value);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        // insert new entry at head
        let new_entry = Entry::new(key, value, hash);
        unsafe {
            (*new_entry)
                .next
                .store(slot.head.load(Ordering::Acquire), Ordering::Release);
        }
        slot.head.store(new_entry, Ordering::Release);
        self.count.fetch_add(1, Ordering::Relaxed);

        slot.mutex.unlock_exclusive();
        None
    }

    fn remove<Q: ?Sized>(&self, hash: u64, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let slot = self.get_slot(hash);
        slot.mutex.lock_exclusive();

        let mut cur = slot.head.load(Ordering::Acquire);
        let mut prev: *mut Entry<K, V> = null_mut();

        while !cur.is_null() {
            unsafe {
                if (*cur).hash == hash && (*cur).key.borrow() == key {
                    let next = (*cur).next.load(Ordering::Acquire);
                    if prev.is_null() {
                        slot.head.store(next, Ordering::Release);
                    } else {
                        (*prev).next.store(next, Ordering::Release);
                    }

                    let value = ManuallyDrop::into_inner(std::ptr::read(&(*cur).value));
                    drop(Box::from_raw(cur));
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

    fn contains<Q: ?Sized>(&self, hash: u64, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let slot = self.get_slot(hash);
        slot.mutex.lock_shared();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).hash == hash && (*cur).key.borrow() == key {
                    slot.mutex.unlock_shared();
                    return true;
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_shared();
        false
    }

    fn clear(&self) {
        // exclusive over the shard because we will mutate every slot
        for slot_p in &self.slots {
            // slot_p: &CachePadded<Slot<K,V>>
            let slot: &Slot<K, V> = &*slot_p;
            slot.mutex.lock_exclusive();
            let mut cur = slot.head.load(Ordering::Acquire);
            while !cur.is_null() {
                unsafe {
                    let next = (*cur).next.load(Ordering::Acquire);
                    ManuallyDrop::drop(&mut (*cur).value);
                    drop(Box::from_raw(cur));
                    cur = next;
                }
            }
            slot.head.store(null_mut(), Ordering::Release);
            slot.mutex.unlock_exclusive();
        }

        self.count.store(0, Ordering::Release);
    }
}

impl<K, V> Shard<K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

impl<K, V> Drop for Shard<K, V> {
    fn drop(&mut self) {
        for slot_p in &self.slots {
            let slot: &Slot<K, V> = &*slot_p;
            let mut cur = slot.head.load(Ordering::Acquire);
            while !cur.is_null() {
                unsafe {
                    let next = (*cur).next.load(Ordering::Acquire);
                    ManuallyDrop::drop(&mut (*cur).value);
                    drop(Box::from_raw(cur));
                    cur = next;
                }
            }
        }
    }
}

/// Inner structure with reference counting
struct Inner<K, V, S> {
    shift: usize,
    /// Shards padded individually to avoid false sharing.
    shards: Box<[SpinLockCell<Shard<K, V>>]>,
    hasher: S,
    /// ref_count padded to avoid false sharing with other atomics.
    ref_count: CachePadded<AtomicUsize>,
}

/// Thread-safe HashMap with sharded storage and reference counting
#[repr(transparent)]
pub struct AtomicHashMap<K, V, S = RandomState> {
    inner: *const Inner<K, V, S>,
}

unsafe impl<K: Send, V: Send, S> Send for AtomicHashMap<K, V, S> {}
unsafe impl<K: Send, V: Send, S> Sync for AtomicHashMap<K, V, S> {}

impl<K, V, S> UnwindSafe for AtomicHashMap<K, V, S> {}
impl<K, V, S> RefUnwindSafe for AtomicHashMap<K, V, S> {}

impl<K: Eq + Hash, V> AtomicHashMap<K, V, RandomState> {
    pub fn new() -> Self {
        Self::with_capacity_and_hasher(0, RandomState::default())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, RandomState::default())
    }

    pub fn with_shard_amount(shard_amount: usize) -> Self {
        Self::with_capacity_hasher_and_shard_amount(0, RandomState::default(), shard_amount)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Self::with_capacity_and_hasher(0, hasher)
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self::with_capacity_hasher_and_shard_amount(capacity, hasher, DEFAULT_SHARD_AMOUNT)
    }

    pub fn with_capacity_hasher_and_shard_amount(
        mut capacity: usize,
        hasher: S,
        shard_amount: usize,
    ) -> Self {
        assert!(shard_amount > 1, "shard_amount must be > 1");
        assert!(
            shard_amount.is_power_of_two(),
            "shard_amount must be power of 2"
        );

        let shift = std::mem::size_of::<usize>() * 8 - shard_amount.trailing_zeros() as usize;

        if capacity != 0 {
            capacity = (capacity + (shard_amount - 1)) & !(shard_amount - 1);
        }

        let capacity_per_shard = if capacity == 0 {
            16
        } else {
            capacity / shard_amount
        };

        let shards_vec: Vec<SpinLockCell<Shard<K, V>>> = (0..shard_amount)
            .map(|_| SpinLockCell::new(Shard::new(capacity_per_shard)))
            .collect();
        let shards = shards_vec.into_boxed_slice();

        let inner = Box::new(Inner {
            shift,
            shards,
            hasher,
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        });

        Self {
            inner: Box::into_raw(inner),
        }
    }

    #[inline]
    fn hash<Q: ?Sized + Hash>(&self, key: &Q) -> u64 {
        let mut hasher = self.inner().hasher.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    #[inline]
    fn determine_shard(&self, hash: u64) -> usize {
        ((hash as usize) << 7) >> self.inner().shift
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let hash = self.hash(&key);
        let idx = self.determine_shard(hash);
        // shards is Box<[CachePadded<Shard<...>>]>, indexing yields &CachePadded<Shard<...>>
        let shard = &self.inner().shards[idx].lock_shared();
        shard.insert(key, value, hash)
    }

    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let idx = self.determine_shard(hash);
        let shard = self.inner().shards[idx].lock_shared(); // prendi solo per trovare lo slot
        let slot = shard.get_slot(hash);

        // Acquisisci il lock sullo slot
        slot.mutex.lock_shared();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).hash == hash && (*cur).key.borrow() == key {
                    // Creiamo un WatchGuardRef solo con il lock del slot
                    return Some(WatchGuardRef::new(&(*cur).value, slot.mutex.clone()));
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        None
    }

    pub fn get_mut<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let idx = self.determine_shard(hash);
        let shard = self.inner().shards[idx].lock_shared();
        let slot = shard.get_slot(hash);
        slot.mutex.lock_exclusive();

        let mut cur = slot.head.load(Ordering::Acquire);
        while !cur.is_null() {
            unsafe {
                if (*cur).hash == hash && (*cur).key.borrow() == key {
                    return Some(WatchGuardMut::new(&mut *(*cur).value, slot.mutex.clone()));
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        slot.mutex.unlock_exclusive();
        None
    }

    pub fn remove<Q: ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let idx = self.determine_shard(hash);
        let shard: &Shard<K, V> = &*self.inner().shards[idx].lock_shared();
        shard.remove(hash, key)
    }

    pub fn contains_key<Q: ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let idx = self.determine_shard(hash);
        let shard = self.inner().shards[idx].lock_shared();
        shard.contains(hash, key)
    }

    pub fn clear(&self) {
        for shard_p in self.inner().shards.iter() {
            let shard = shard_p.lock_exclusive();
            shard.clear();
        }
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.inner().hasher
    }

    pub fn as_vec<KC, VC>(&self) -> Vec<(KC, VC)>
    where
        K: Clone + Into<KC>,
        V: Clone + Into<VC>,
    {
        let mut result = Vec::with_capacity(self.len());

        for shard_p in self.inner().shards.iter() {
            let shard = shard_p.lock_shared();

            for slot_p in &shard.slots {
                let slot: &Slot<K, V> = &*slot_p;
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

        result
    }
}

// Methods without generic constraints
impl<K, V, S> AtomicHashMap<K, V, S> {
    #[inline]
    fn inner(&self) -> &Inner<K, V, S> {
        unsafe { &*self.inner }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner().shards.iter().map(|s| s.lock_shared().len()).sum()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.inner().shards.iter().map(|s| s.lock_shared().capacity).sum()
    }
}

impl<K, V, S> Clone for AtomicHashMap<K, V, S> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { inner: self.inner }
    }
}

impl<K, V, S> Drop for AtomicHashMap<K, V, S> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            unsafe {
                drop(Box::from_raw(self.inner as *mut Inner<K, V, S>));
            }
        }
    }
}

impl<K, V, S> fmt::Debug for AtomicHashMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicHashMap")
            .field("shards", &self.inner().shards.len())
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
