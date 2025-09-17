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
}

impl<K, V> Shard<K, V> {
    fn new(size: usize) -> Self {
        let slots = AtomicVec::with_capacity(size);
        for _ in 0..size {
            let slot = Box::into_raw(Box::new(Slot::new()));
            slots.push(slot);
        }
        Self {
            slots,
            size: AtomicUsize::new(size),
        }
    }

    #[inline]
    fn get_slot(&self, index: usize) -> Option<&Slot<K, V>> {
        self.slots.get(index).map(|ptr| unsafe { &*ptr })
    }

    fn cleanup(&self) {
        let size = self.size.load(Ordering::Acquire);
        for i in 0..size {
            if let Some(slot_ptr) = self.slots.get(i) {
                unsafe {
                    let slot = &*slot_ptr;
                    let mut cur = slot.head.load(Ordering::Acquire);
                    while !cur.is_null() {
                        let next = (*cur).next.load(Ordering::Acquire);
                        ManuallyDrop::drop(&mut (*cur).value);
                        drop(Box::from_raw(cur));
                        cur = next;
                    }
                    drop(Box::from_raw(slot_ptr));
                }
            }
        }
    }
}

/// Inner structure containing all shards and metadata
struct AtomicInner<K, V, S> {
    /// Array of shards
    shards: Vec<Shard<K, V>>,
    /// Total number of entries
    len: CachePadded<AtomicUsize>,
    /// Reference counter
    ref_count: CachePadded<AtomicUsize>,
    /// Hash builder
    hasher: S,
    /// Global lock for operations like resize
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
    /// Create a new AtomicHashMap with default configuration
    pub fn new() -> Self {
        Self::with_shard_size(DEFAULT_SHARD_SIZE)
    }

    /// Create a new AtomicHashMap with specified shard size
    pub fn with_shard_size(shard_size: usize) -> Self {
        Self::with_hasher_and_shard_size(RandomState::default(), shard_size)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Clone> AtomicHashMap<K, V, S> {
    /// Create with custom hasher and shard size
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

    /// Hash a key
    fn hash<Q: ?Sized + Hash>(&self, key: &Q) -> u64 {
        let mut hasher = self.inner().hasher.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Get shard index and slot index from hash
    #[inline]
    fn get_indices(&self, hash: u64) -> (usize, usize) {
        let shard_idx = (hash as usize) % NUM_SHARDS;
        let shard_size = self.inner().shards[shard_idx].size.load(Ordering::Relaxed);
        let slot_idx = ((hash >> 32) as usize) % shard_size;
        (shard_idx, slot_idx)
    }

    /// Insert a key-value pair
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let hash = self.hash(&key);
        let (shard_idx, slot_idx) = self.get_indices(hash);

        let shard = &self.inner().shards[shard_idx];
        let slot = shard.get_slot(slot_idx)?;

        // Lock the specific slot
        slot.mutex.lock_exclusive();

        // Check if key exists in collision chain
        let mut cur = slot.head.load(Ordering::Acquire);

        while !cur.is_null() {
            unsafe {
                if (*cur).key == key {
                    // Replace existing value
                    let old_value = ManuallyDrop::into_inner(std::ptr::read(&(*cur).value));
                    (*cur).value = ManuallyDrop::new(value);
                    slot.mutex.unlock_exclusive();
                    return Some(old_value);
                }
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }

        // Insert new entry at head
        let new_entry = Entry::new(key, value);
        unsafe {
            (*new_entry)
                .next
                .store(slot.head.load(Ordering::Acquire), Ordering::Release);
        }
        slot.head.store(new_entry, Ordering::Release);

        self.inner().len.fetch_add(1, Ordering::Relaxed);
        slot.mutex.unlock_exclusive();

        None
    }

    /// Get a value by key (returns a read guard)
    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardRef<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);

        let shard = &self.inner().shards[shard_idx];
        let slot = shard.get_slot(slot_idx)?;

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

    /// Get a mutable value by key (returns a write guard)
    pub fn get_mut<Q: ?Sized>(&self, key: &Q) -> Option<WatchGuardMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);

        let shard = &self.inner().shards[shard_idx];
        let slot = shard.get_slot(slot_idx)?;

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

    /// Remove a key-value pair
    pub fn remove<Q: ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);

        let shard = &self.inner().shards[shard_idx];
        let slot = shard.get_slot(slot_idx)?;

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

                    self.inner().len.fetch_sub(1, Ordering::Relaxed);
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

    /// Clear all entries
    pub fn clear(&self) {
        self.inner().global_lock.lock_exclusive();

        for shard in &self.inner().shards {
            let size = shard.size.load(Ordering::Acquire);
            for i in 0..size {
                if let Some(slot) = shard.get_slot(i) {
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
            }
        }

        self.inner().len.store(0, Ordering::Release);
        self.inner().global_lock.unlock_exclusive();
    }

    /// Check if a key exists
    pub fn contains_key<Q: ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = self.hash(key);
        let (shard_idx, slot_idx) = self.get_indices(hash);

        let shard = &self.inner().shards[shard_idx];
        if let Some(slot) = shard.get_slot(slot_idx) {
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
        }
        false
    }

    /// Get hasher
    pub fn hasher(&self) -> &S {
        &self.inner().hasher
    }

    pub fn iter(&self) -> Iter<'_, K, V, S> {
        Iter::new(self)
    }
}

impl<K, V, S> AtomicHashMap<K, V, S> {
    #[inline(always)]
    fn inner(&self) -> &AtomicInner<K, V, S> {
        unsafe { &*self.ptr }
    }

    /// Get the number of entries
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    /// Check if empty
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

            // Clean up all shards
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

/// Iteratore per AtomicHashMap basato su shards/slots
pub struct Iter<'a, K, V, S> {
    map: &'a AtomicHashMap<K, V, S>,
    shard_idx: usize,
    slot_idx: usize,
    current: *mut Entry<K, V>,
    locked_slot: Option<(*const Slot<K, V>, usize, usize)>, // (ptr, shard_idx, slot_idx)
}

impl<'a, K: Eq + Hash, V, S: BuildHasher + Clone> Iter<'a, K, V, S> {
    pub fn new(map: &'a AtomicHashMap<K, V, S>) -> Self {
        // Acquisisce subito il lock globale in modalità shared (come faceva la versione vecchia)
        unsafe {
            (&*map.ptr).global_lock.lock_shared();
        }

        let mut it = Iter {
            map,
            shard_idx: 0,
            slot_idx: 0,
            current: null_mut(),
            locked_slot: None,
        };

        it.advance_slot(); // posizionati sul primo slot non vuoto (se presente)
        it
    }

    fn advance_slot(&mut self) {
        // rilascia lock del slot corrente (se presente)
        if let Some((slot_ptr, _, _)) = self.locked_slot.take() {
            unsafe {
                (&*slot_ptr).mutex.unlock_shared();
            }
        }

        self.current = null_mut();

        // scorri shards e slots fino a trovare un head non nullo
        while self.shard_idx < self.map.inner().shards.len() {
            let shard = &self.map.inner().shards[self.shard_idx];
            let shard_size = shard.size.load(Ordering::Acquire);

            while self.slot_idx < shard_size {
                if let Some(slot_ref) = shard.get_slot(self.slot_idx) {
                    let head = slot_ref.head.load(Ordering::Acquire);
                    if !head.is_null() {
                        // lock condiviso sullo slot trovato e memorizza la posizione
                        slot_ref.mutex.lock_shared();
                        self.current = head;
                        self.locked_slot = Some((slot_ref as *const Slot<K, V>, self.shard_idx, self.slot_idx));
                        return;
                    }
                }
                self.slot_idx += 1;
            }

            // passa al prossimo shard
            self.shard_idx += 1;
            self.slot_idx = 0;
        }

        // se qui, non trovato alcuno slot non vuoto (current resta null)
    }
}

impl<'a, K: Eq + Hash, V, S: BuildHasher + Clone> Iterator for Iter<'a, K, V, S> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        // se siamo già oltre l'ultimo shard -> fine
        if self.shard_idx >= self.map.inner().shards.len() {
            return None;
        }

        if self.current.is_null() {
            return None;
        }

        unsafe {
            let entry = &*self.current;
            // prendi la prossima entry nella catena
            self.current = entry.next.load(Ordering::Acquire);

            // se la lista corrente è finita, avanza allo slot successivo
            if self.current.is_null() {
                // incrementa gli indici per cercare il prossimo slot valido
                if let Some((_, _shard_idx, slot_idx)) = self.locked_slot {
                    // il campo slot_idx nella struttura iteratora rimane quello corrente; incrementalo
                    // (attenzione: abbiamo memorizzato slot_idx nello stato dell'iteratore stesso)
                    // passiamo al prossimo slot del medesimo shard
                    let next_slot = slot_idx + 1;
                    self.slot_idx = next_slot;
                } else {
                    // caso in cui locked_slot non è presente (ma current era null),
                    // passiamo semplicemente a slot_idx+1 per cercare successivamente
                    self.slot_idx += 1;
                }

                // se lo slot corrente era l'ultimo dello shard, advance_slot gestirà il cambio shard
                self.advance_slot();
            }

            // ritorna riferimenti al key/value correnti
            Some((&entry.key, &*entry.value))
        }
    }
}

impl<'a, K, V, S> Drop for Iter<'a, K, V, S> {
    fn drop(&mut self) {
        // rilascia slot lockato, se presente
        if let Some((slot_ptr, _, _)) = self.locked_slot.take() {
            unsafe {
                (&*slot_ptr).mutex.unlock_shared();
            }
        }

        // sblocca il lock globale acquisito in Iter::new()
        unsafe {
            (&*self.map.ptr).global_lock.unlock_shared();
        }
    }
}