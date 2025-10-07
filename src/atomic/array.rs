use crate::mutex::{Backoff, Mutex, WatchGuardMut, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

/// Default capacity for the array
const DEFAULT_ARRAY_CAP: usize = 32;

/// Slot state flags (u32 for memory efficiency and cache usage)
const EMPTY: usize = 0;
const WRITE: usize = 1;
const READ: usize = 2;

/// Represents a single slot in the array.
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicUsize,
    lock: Mutex,
}

impl<T> Slot<T> {
    #[inline(always)]
    fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicUsize::new(EMPTY),
            lock: Mutex::new(),
        }
    }

    /// Wait until this slot is written, with fast-path check and backoff spin
    #[inline(always)]
    fn wait_write(&self) {
        if self.state.load(Ordering::Acquire) & WRITE != 0 {
            return;
        }
        let backoff = Backoff::new();
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.snooze();
        }
    }

    /// Reset slot to EMPTY state
    #[inline(always)]
    fn reset(&self) {
        self.state.store(EMPTY, Ordering::Relaxed);
    }

    /// Return true if slot contains written data
    #[inline(always)]
    fn is_written(&self) -> bool {
        self.state.load(Ordering::Relaxed) & WRITE != 0
    }
}

/// Internal structure representing the array
struct InnerArray<T> {
    slots: *mut Slot<T>,
    capacity: usize,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    len: CachePadded<AtomicUsize>,
    ref_count: CachePadded<AtomicUsize>,
}

/// Thread-safe atomic array wrapper
#[repr(transparent)]
pub struct AtomicArray<T> {
    inner: *const InnerArray<T>,
}

unsafe impl<T: Send> Send for AtomicArray<T> {}
unsafe impl<T: Send> Sync for AtomicArray<T> {}

impl<T> AtomicArray<T> {
    /// Create new array with default capacity
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_ARRAY_CAP)
    }

    /// Create new array with specific capacity
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be greater than 0");

        // Allocate memory for slots (cache-aligned)
        let layout = std::alloc::Layout::from_size_align(
            capacity * mem::size_of::<Slot<T>>(),
            mem::align_of::<Slot<T>>(),
        )
        .expect("Failed to create layout");

        let slots = unsafe {
            let ptr = std::alloc::alloc_zeroed(layout) as *mut Slot<T>;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            for i in 0..capacity {
                ptr.add(i).write(Slot::new());
            }
            ptr
        };

        let inner = InnerArray {
            slots,
            capacity,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            len: CachePadded::new(AtomicUsize::new(0)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        };

        Self {
            inner: Box::into_raw(Box::new(inner)),
        }
    }

    /// Initialize array with given capacity and initializer function
    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let arr = Self::with_capacity(cap);
        for _ in 0..cap {
            let _ = arr.push(initializer());
        }
        arr
    }

    #[inline(always)]
    fn inner(&self) -> &InnerArray<T> {
        unsafe { &*self.inner }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.inner().capacity
    }

    /// Push a new element (fast path)
    #[inline]
    pub fn push(&self, value: T) -> Result<(), T> {
        let inner = self.inner();
        let tail = inner.tail.load(Ordering::Relaxed);
        if tail >= inner.capacity {
            return Err(value);
        }

        if inner
            .tail
            .compare_exchange(tail, tail + 1, Ordering::Release, Ordering::Acquire)
            .is_ok()
        {
            unsafe {
                let slot = &*inner.slots.add(tail);
                ptr::write(slot.value.get(), MaybeUninit::new(value));
                slot.state.store(WRITE, Ordering::Release);
                inner.len.fetch_add(1, Ordering::Release);
            }
            return Ok(());
        }

        self.push_slow(value)
    }

    /// Slow path push using backoff
    #[cold]
    fn push_slow(&self, value: T) -> Result<(), T> {
        let inner = self.inner();
        let backoff = Backoff::new();
        let mut tail = inner.tail.load(Ordering::Relaxed);

        loop {
            if tail >= inner.capacity {
                return Err(value);
            }
            match inner
                .tail
                .compare_exchange(tail, tail + 1, Ordering::Release, Ordering::Acquire)
            {
                Ok(_) => {
                    unsafe {
                        let slot = &*inner.slots.add(tail);
                        ptr::write(slot.value.get(), MaybeUninit::new(value));
                        slot.state.store(WRITE, Ordering::Release);
                        inner.len.fetch_add(1, Ordering::Release);
                    }
                    return Ok(());
                }
                Err(t) => {
                    tail = t;
                    backoff.snooze();
                }
            }
        }
    }

    /// Access element by index (shared)
    #[inline]
    pub fn get(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        let inner = self.inner();
        let len = inner.len.load(Ordering::Acquire);
        if index >= len {
            return None;
        }

        let target = inner.head.load(Ordering::Acquire) + index;
        if target >= inner.capacity {
            return None;
        }

        unsafe {
            let slot = &*inner.slots.add(target);
            slot.wait_write();
            slot.lock.lock_shared();
            Some(WatchGuardRef::new(
                (*slot.value.get()).assume_init_ref(),
                slot.lock.clone(),
            ))
        }
    }

    /// Access element by index (mutable/exclusive)
    #[inline]
    pub fn get_mut(&self, index: usize) -> Option<WatchGuardMut<'_, T>> {
        let inner = self.inner();
        let len = inner.len.load(Ordering::Acquire);
        if index >= len {
            return None;
        }

        let target = inner.head.load(Ordering::Acquire) + index;
        if target >= inner.capacity {
            return None;
        }

        unsafe {
            let slot = &*inner.slots.add(target);
            slot.wait_write();
            slot.lock.lock_exclusive();
            Some(WatchGuardMut::new(
                (*slot.value.get()).assume_init_mut(),
                slot.lock.clone(),
            ))
        }
    }

    /// Reset array with new capacity and initializer
    pub fn reset_with(
        &self,
        new_cap: usize,
        mut initializer: impl FnMut() -> T,
    ) -> Result<usize, usize> {
        assert!(new_cap > 0, "Capacity must be greater than 0");

        let inner_ptr = self.inner as *mut InnerArray<T>;
        let inner = unsafe { &mut *inner_ptr };
        let old_capacity = inner.capacity;
        let old_slots = inner.slots;

        // Lock all old slots exclusively
        unsafe {
            for i in 0..old_capacity {
                let slot = &*old_slots.add(i);
                if slot.is_written() {
                    slot.wait_write();
                    slot.lock.lock_exclusive();
                }
            }
        }

        // Drop old values if necessary
        if mem::needs_drop::<T>() {
            unsafe {
                for i in 0..old_capacity {
                    let slot = &*old_slots.add(i);
                    if slot.is_written() {
                        ptr::drop_in_place(slot.value.get());
                    }
                }
            }
        }

        // Deallocate old slots
        unsafe {
            for i in 0..old_capacity {
                ptr::drop_in_place(old_slots.add(i));
            }
            let old_layout = std::alloc::Layout::from_size_align(
                old_capacity * mem::size_of::<Slot<T>>(),
                mem::align_of::<Slot<T>>(),
            )
            .expect("Failed to create layout");
            std::alloc::dealloc(old_slots as *mut u8, old_layout);
        }

        // Allocate new slots
        let layout = std::alloc::Layout::from_size_align(
            new_cap * mem::size_of::<Slot<T>>(),
            mem::align_of::<Slot<T>>(),
        )
        .expect("Failed to create layout");

        let slots = unsafe {
            let ptr = std::alloc::alloc_zeroed(layout) as *mut Slot<T>;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            for i in 0..new_cap {
                ptr.add(i).write(Slot::new());
            }
            ptr
        };

        inner.slots = slots;
        inner.capacity = new_cap;
        inner.head.store(0, Ordering::Relaxed);
        inner.tail.store(0, Ordering::Relaxed);
        inner.len.store(0, Ordering::Relaxed);

        // Initialize new values
        let inner_immut = unsafe { &*inner_ptr };
        for i in 0..new_cap {
            let value = initializer();
            unsafe {
                let slot = &*inner_immut.slots.add(i);
                ptr::write(slot.value.get(), MaybeUninit::new(value));
                slot.state.store(WRITE, Ordering::Release);
            }
            inner_immut.tail.store(i + 1, Ordering::Relaxed);
            inner_immut.len.store(i + 1, Ordering::Relaxed);
        }

        Ok(new_cap)
    }

    /// Convert to Vec<T> (requires Clone)
    pub fn as_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        let len = self.len();
        if len == 0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(len);
        let inner = self.inner();
        let head = inner.head.load(Ordering::Acquire);

        unsafe {
            for i in 0..len {
                let target = head + i;
                if target < inner.capacity {
                    let slot = &*inner.slots.add(target);
                    slot.wait_write();
                    out.push((*slot.value.get()).assume_init_ref().clone());
                }
            }
        }

        out
    }

    /// Apply shared function to each element
    #[inline]
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&T),
    {
        let inner = self.inner();
        let len = inner.len.load(Ordering::Acquire);
        let head = inner.head.load(Ordering::Acquire);

        unsafe {
            for i in 0..len {
                let slot = &*inner.slots.add(head + i);
                slot.wait_write();
                slot.lock.lock_shared();
                f((*slot.value.get()).assume_init_ref());
                slot.lock.unlock_shared();
            }
        }
    }

    /// Apply exclusive mutable function to each element
    #[inline]
    pub fn for_each_mut<F>(&self, mut f: F)
    where
        F: FnMut(&mut T),
    {
        let inner = self.inner();
        let len = inner.len.load(Ordering::Acquire);
        let head = inner.head.load(Ordering::Acquire);

        unsafe {
            for i in 0..len {
                let slot = &*inner.slots.add(head + i);
                slot.wait_write();
                slot.lock.lock_exclusive();
                f((*slot.value.get()).assume_init_mut());
                slot.lock.unlock_exclusive();
            }
        }
    }

    /// Split indices into chunks for parallel processing
    #[inline]
    pub fn chunk_indices(&self, num_chunks: usize) -> Vec<(usize, usize)> {
        let len = self.len();
        if len == 0 || num_chunks == 0 {
            return vec![];
        }

        let chunk_size = (len + num_chunks - 1) / num_chunks;
        let mut chunks = Vec::with_capacity(num_chunks);
        for i in 0..num_chunks {
            let start = i * chunk_size;
            let end = ((i + 1) * chunk_size).min(len);
            if start < len {
                chunks.push((start, end));
            }
        }
        chunks
    }
}

impl<T> Clone for AtomicArray<T> {
    #[inline]
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { inner: self.inner }
    }
}

impl<T> Drop for AtomicArray<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.inner };
        if inner.ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);

        // Drop all values if necessary
        unsafe {
            if mem::needs_drop::<T>() {
                for i in 0..inner.capacity {
                    let slot = &*inner.slots.add(i);
                    if slot.is_written() {
                        ptr::drop_in_place(slot.value.get());
                    }
                }
            }

            // Drop slots
            for i in 0..inner.capacity {
                ptr::drop_in_place(inner.slots.add(i));
            }

            let layout = std::alloc::Layout::from_size_align(
                inner.capacity * mem::size_of::<Slot<T>>(),
                mem::align_of::<Slot<T>>(),
            )
            .expect("Failed to create layout");
            std::alloc::dealloc(inner.slots as *mut u8, layout);

            drop(Box::from_raw(self.inner as *mut InnerArray<T>));
        }
    }
}

impl<T> FromIterator<T> for AtomicArray<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let capacity = iter.size_hint().0.max(DEFAULT_ARRAY_CAP);
        let arr = Self::with_capacity(capacity);
        for item in iter {
            let _ = arr.push(item);
        }
        arr
    }
}

impl<T> Default for AtomicArray<T> {

    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicArray<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicArray")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}
