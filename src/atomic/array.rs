use crate::mutex::{Backoff, Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

// Default array capacity
const DEFAULT_ARRAY_CAP: usize = 64;

// Slot state flags
const WRITE: usize = 1; // slot has been written
const READ: usize = 2; // slot has been read

/// Represents a single slot in the array.
/// UnsafeCell allows interior mutability without borrowing restrictions.
/// The state is atomic to allow concurrent access.
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicUsize,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicUsize::new(0),
        }
    }

    #[inline(always)]
    fn wait_write(&self) {
        let backoff = Backoff::new();
        // Wait until the slot is written by a producer
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.snooze();
        }
    }

    #[inline]
    fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
    }
}

/// The internal representation of the static array.
#[repr(C)]
struct InnerArray<T> {
    slots: *mut Slot<T>,                 // pointer to allocated slots
    capacity: usize,                     // current capacity
    head: CachePadded<AtomicUsize>,      // head index
    tail: CachePadded<AtomicUsize>,      // tail index
    len: CachePadded<AtomicUsize>,       // current length
    ref_count: CachePadded<AtomicUsize>, // reference count for cloning
    lock: CachePadded<AtomicUsize>,      // shared/exclusive lock
}

/// Thread-safe static array with atomic operations.
#[repr(transparent)]
pub struct AtomicArray<T> {
    inner: *const InnerArray<T>,
}

unsafe impl<T: Send> Send for AtomicArray<T> {}
unsafe impl<T: Send> Sync for AtomicArray<T> {}

impl<T> AtomicArray<T> {
    /// Creates a new empty atomic array with default capacity.
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_ARRAY_CAP)
    }

    /// Creates a new empty atomic array with specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be greater than 0");

        // Allocate slots array
        let layout = std::alloc::Layout::array::<Slot<T>>(capacity)
            .expect("Failed to create layout");
        let slots = unsafe {
            let ptr = std::alloc::alloc_zeroed(layout) as *mut Slot<T>;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            // Initialize all slots
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
            lock: CachePadded::new(AtomicUsize::new(0)),
        };

        Self {
            inner: Box::into_raw(Box::new(inner)),
        }
    }

    /// Initializes the array with a given capacity using a provided initializer.
    /// Resizes the array if needed.
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

    /// Acquire a shared lock (readers) with backoff
    #[inline(always)]
    fn wait_lock_shared(&self) {
        let inner = self.inner();
        let backoff = Backoff::new();

        // increment readers by 2 (lowest bit reserved for exclusive lock)
        let prev = inner.lock.fetch_add(2, Ordering::AcqRel);
        if prev & 1 == 1 {
            // Wait until exclusive lock is released
            while inner.lock.load(Ordering::Acquire) & 1 == 1 {
                backoff.snooze();
            }
        }
    }

    /// Acquire an exclusive lock (writers) with backoff
    #[inline(always)]
    fn wait_lock_exclusive(&self) {
        let inner = self.inner();
        let backoff = Backoff::new();

        loop {
            match inner
                .lock
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break, // success
                Err(_) => backoff.snooze(),
            }
        }
    }

    #[inline(always)]
    fn release_lock_shared(&self) {
        self.inner().lock.fetch_sub(2, Ordering::Release);
    }

    #[inline(always)]
    fn release_lock_exclusive(&self) {
        self.inner().lock.store(0, Ordering::Release);
    }

    /// Push a value without acquiring the shared lock.
    /// Unsafe because concurrent access may occur if called externally.
    /// Returns an error if the array is full.
    pub unsafe fn push_unchecked(&self, value: T) -> Result<(), T> {
        let inner = self.inner();
        let backoff = Backoff::new();
        let mut tail = inner.tail.load(Ordering::Acquire);

        loop {
            // Check if array is full
            if tail >= inner.capacity {
                return Err(value);
            }

            let new_tail = tail + 1;

            // Atomically claim the next slot
            match inner.tail.compare_exchange(
                tail,
                new_tail,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Write the value to the slot
                    let slot = &*inner.slots.add(tail);
                    slot.value.get().write(MaybeUninit::new(value));
                    // Publish the WRITE state to signal readers
                    slot.state.store(WRITE, Ordering::Release);

                    // Increment the vector length
                    inner.len.fetch_add(1, Ordering::Release);
                    return Ok(());
                }
                Err(t) => {
                    // CAS failed, reload tail and retry
                    tail = t;
                    backoff.snooze();
                }
            }
        }
    }

    /// Thread-safe push with shared lock.
    /// Returns an error if the array is full.
    pub fn push(&self, value: T) -> Result<(), T> {
        self.wait_lock_shared();
        let result = unsafe { self.push_unchecked(value) };
        self.release_lock_shared();
        result
    }

    unsafe fn get_unchecked(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        let inner = self.inner();

        if index >= self.len() {
            return None;
        }

        let head_idx = inner.head.load(Ordering::Acquire);
        let target = head_idx + index;

        if target >= inner.capacity {
            return None;
        }

        let slot = &*inner.slots.add(target);
        slot.wait_write();

        Some(WatchGuardRef::new(
            (*slot.value.get()).assume_init_ref(),
            Mutex::new(),
        ))
    }

    /// Get a reference to the value at a specific index.
    /// Returns a watch guard to allow safe concurrent access.
    #[inline]
    pub fn get(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        self.wait_lock_shared();
        let val = unsafe { self.get_unchecked(index) };
        self.release_lock_shared();
        val
    }

    /// Reset the array with new capacity and initialize using a provided initializer.
    /// This will deallocate the old array and allocate a new one with the specified capacity.
    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> Result<usize, usize> {
        assert!(new_cap > 0, "Capacity must be greater than 0");

        let inner = unsafe { &mut *(self.inner as *mut InnerArray<T>) };
        self.wait_lock_exclusive();

        // Drop existing elements if needed
        if mem::needs_drop::<T>() {
            let len = inner.len.load(Ordering::Relaxed);
            let head = inner.head.load(Ordering::Relaxed);
            unsafe {
                for i in 0..len {
                    let idx = head + i;
                    if idx < inner.capacity {
                        let slot = &*inner.slots.add(idx);
                        if slot.state.load(Ordering::Relaxed) & WRITE != 0 {
                            ptr::drop_in_place(slot.value.get());
                        }
                    }
                }
            }
        }

        // Deallocate old slots
        unsafe {
            let old_layout = std::alloc::Layout::array::<Slot<T>>(inner.capacity)
                .expect("Failed to create layout");
            std::alloc::dealloc(inner.slots as *mut u8, old_layout);
        }

        // Allocate new slots array
        let layout = std::alloc::Layout::array::<Slot<T>>(new_cap)
            .expect("Failed to create layout");
        let slots = unsafe {
            let ptr = std::alloc::alloc_zeroed(layout) as *mut Slot<T>;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            // Initialize all slots
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

        unsafe {
            for i in 0..new_cap {
                if self.push_unchecked(initializer()).is_err() {
                    self.release_lock_exclusive();
                    return Err(i);
                }
            }
        }

        self.release_lock_exclusive();
        Ok(new_cap)
    }

    /// Convert the atomic array into a standard Vec<T>.
    /// Consumes all elements, thread-safely.
    pub fn as_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        let mut out = Vec::with_capacity(self.len());
        if self.is_empty() {
            return out;
        }

        self.wait_lock_shared();
        for pos in 0..self.len() {
            if let Some(guard) = unsafe { self.get_unchecked(pos) } {
                out.push((*guard).clone());
            }
        }
        self.release_lock_shared();
        out
    }
}

impl<T> Clone for AtomicArray<T> {
    /// Cloning the AtomicArray only increases the reference count.
    /// The underlying data is shared safely between clones.
    fn clone(&self) -> Self {
        let inner = self.inner();
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        Self { inner: self.inner }
    }
}

impl<T> Drop for AtomicArray<T> {
    /// Drops the AtomicArray.
    /// If this is the last reference, deallocates the inner structure.
    fn drop(&mut self) {
        let inner = unsafe { &*self.inner };

        // Only deallocate if this is the last reference
        if inner.ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);

        unsafe {
            // Drop all written elements if T needs drop
            if mem::needs_drop::<T>() {
                for i in 0..inner.capacity {
                    let slot = &*inner.slots.add(i);
                    if slot.state.load(Ordering::Relaxed) & WRITE != 0 {
                        ptr::drop_in_place(slot.value.get());
                    }
                }
            }

            // Deallocate slots array
            let layout = std::alloc::Layout::array::<Slot<T>>(inner.capacity)
                .expect("Failed to create layout");
            std::alloc::dealloc(inner.slots as *mut u8, layout);

            // Deallocate InnerArray itself
            drop(Box::from_raw(self.inner as *mut InnerArray<T>));
        }
    }
}

impl<T> FromIterator<T> for AtomicArray<T> {
    /// Creates an AtomicArray from an iterator of items.
    /// Allocates capacity based on size hint if available, otherwise uses default.
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
    /// Creates a new empty AtomicArray
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicArray<T> {
    /// Implements Debug for AtomicArray.
    /// Displays the current length and capacity.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicArray")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}