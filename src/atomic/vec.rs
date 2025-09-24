use crate::mutex::{Backoff, Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::alloc::{alloc_zeroed, handle_alloc_error, Layout};
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::{self, MaybeUninit};
use std::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};

/// Block capacity - power of 2 for fast modulo with bitwise AND
const BLOCK_CAP: usize = 32;
const BLOCK_CAP_MASK: usize = BLOCK_CAP - 1; // 31 for fast modulo
const BLOCK_SHIFT: u32 = 5; // log2(32) for fast division

// States for lock-free block allocation
const AVAILABLE: usize = 1;
const BUSY: usize = 2;

/// Ultra-minimal slot - zero abstraction overhead
#[repr(transparent)]
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    #[inline(always)]
    unsafe fn write_unchecked(&self, value: T) {
        (*self.value.get()).write(value);
    }

    #[inline(always)]
    unsafe fn read_unchecked(&self) -> T {
        (*self.value.get()).assume_init_read()
    }

    #[inline(always)]
    unsafe fn get_ref_unchecked(&self) -> &T {
        (*self.value.get()).assume_init_ref()
    }
}

/// Optimized block with better memory layout
#[repr(C)]
struct Block<T> {
    next: AtomicPtr<Block<T>>, // Remove CachePadded overhead
    slots: [Slot<T>; BLOCK_CAP],
}

// Ultra-fast bit manipulation instead of division/modulo
#[inline(always)]
const fn block_index(pos: usize) -> usize {
    pos >> BLOCK_SHIFT
}

#[inline(always)]
const fn index_in_block(pos: usize) -> usize {
    pos & BLOCK_CAP_MASK
}

impl<T> Block<T> {
    const LAYOUT: Layout = {
        let layout = Layout::new::<Self>();
        layout
    };

    #[inline]
    fn new() -> *mut Self {
        let ptr = unsafe { alloc_zeroed(Self::LAYOUT) };
        if ptr.is_null() {
            handle_alloc_error(Self::LAYOUT)
        }
        ptr.cast()
    }

    #[inline(always)]
    unsafe fn dealloc(ptr: *mut Self) {
        drop(Box::from_raw(ptr));
    }
}

/// Optimized position cache with better locality
#[repr(C)]
struct Position<T> {
    pos: AtomicUsize,
    ptr: AtomicPtr<Block<T>>,
}

/// Highly optimized inner structure - minimal atomic operations
#[repr(C)]
struct InnerVec<T> {
    // Hot path fields first for better cache locality
    head: AtomicUsize,        // Read position
    tail: AtomicUsize,        // Write position
    len: AtomicUsize,         // Current length
    cap: AtomicUsize,         // Current capacity

    // Cold path fields
    buf: *mut Block<T>,                    // First block
    buf_tail: AtomicPtr<Block<T>>,         // Last block
    state: AtomicUsize,                    // Allocation state
    ref_count: AtomicUsize,                // Reference counting

    // Position caches for block traversal optimization
    read_cache: Position<T>,
    write_cache: Position<T>,
}

/// Zero-cost abstraction atomic vector
#[repr(transparent)]
pub struct AtomicVec<T> {
    inner: *const InnerVec<T>,
}

unsafe impl<T: Send> Send for AtomicVec<T> {}
unsafe impl<T: Send> Sync for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    #[inline]
    pub fn new() -> Self {
        let inner = Box::into_raw(Box::new(InnerVec::new()));
        Self { inner }
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        let vec = Self::new();
        let inner = vec.inner();

        // Pre-allocate blocks
        let blocks_needed = (cap + BLOCK_CAP_MASK) >> BLOCK_SHIFT;
        for _ in 1..blocks_needed {
            inner.add_block_cold();
        }

        vec
    }

    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::with_capacity(cap);
        let inner = vec.inner();

        // Sequential initialization - no synchronization needed
        unsafe {
            for i in 0..cap {
                inner.get_write_slot_unchecked(i).write_unchecked(initializer());
            }
        }

        inner.head.store(0, Ordering::Relaxed);
        inner.tail.store(cap, Ordering::Relaxed);
        inner.len.store(cap, Ordering::Relaxed);
        vec
    }

    #[inline(always)]
    fn inner(&self) -> &InnerVec<T> {
        unsafe { &*self.inner }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.inner().cap.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ultra-optimized push - minimal atomic operations
    #[inline]
    pub fn push(&self, value: T) {
        let inner = self.inner();

        // Fast path: get write position atomically
        let write_pos = inner.tail.fetch_add(1, Ordering::Relaxed);
        let cap = inner.cap.load(Ordering::Acquire);

        // Check if we need more capacity
        if write_pos >= cap {
            inner.ensure_capacity(write_pos + 1);
        }

        // Write value - no additional synchronization needed
        unsafe {
            inner.get_write_slot_fast(write_pos).write_unchecked(value);
        }

        // Update length after write
        inner.len.fetch_add(1, Ordering::Release);
    }

    /// Ultra-optimized pop - minimal atomic operations
    #[inline]
    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();

        // Fast length check
        let mut current_len = inner.len.load(Ordering::Acquire);
        if current_len == 0 {
            return None;
        }

        // Try to reserve one element atomically
        loop {
            match inner.len.compare_exchange_weak(
                current_len,
                current_len - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(0) => return None, // Became empty
                Err(x) => current_len = x,
            }
        }

        // Get read position atomically
        let read_pos = inner.head.fetch_add(1, Ordering::Relaxed);

        // Read value
        unsafe {
            Some(inner.get_read_slot_fast(read_pos).read_unchecked())
        }
    }

    /// Optimized indexed access
    #[inline]
    pub fn get(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        let inner = self.inner();

        // Fast bounds check
        if index >= inner.len.load(Ordering::Acquire) {
            return None;
        }

        let head = inner.head.load(Ordering::Acquire);
        let cap = inner.cap.load(Ordering::Acquire);
        let pos = (head + index) & (cap - 1); // Assume cap is power of 2

        unsafe {
            Some(WatchGuardRef::new(
                inner.get_slot_fast(pos).get_ref_unchecked(),
                Mutex::new(),
            ))
        }
    }

    /// Optimized reset
    #[inline]
    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> usize {
        let inner = self.inner();

        // Reset atomics
        inner.len.store(0, Ordering::Relaxed);
        inner.head.store(0, Ordering::Relaxed);
        inner.tail.store(0, Ordering::Relaxed);

        // Ensure capacity
        inner.ensure_capacity(new_cap);

        // Fill sequentially
        unsafe {
            for i in 0..new_cap {
                inner.get_write_slot_unchecked(i).write_unchecked(initializer());
            }
        }

        inner.len.store(new_cap, Ordering::Relaxed);
        inner.tail.store(new_cap, Ordering::Release);
        new_cap
    }

    /// Ultra-fast drain to vector
    #[inline]
    pub fn as_vec(&self) -> Vec<T> {
        let inner = self.inner();
        let head = inner.head.load(Ordering::Acquire);
        let len = inner.len.load(Ordering::Acquire);
        let cap = inner.cap.load(Ordering::Acquire);

        let mut out = Vec::with_capacity(len);

        unsafe {
            for i in 0..len {
                let pos = (head + i) & (cap - 1);
                out.push(inner.get_read_slot_fast(pos).read_unchecked());
            }
        }

        // Reset
        inner.head.store(0, Ordering::Relaxed);
        inner.tail.store(0, Ordering::Relaxed);
        inner.len.store(0, Ordering::Release);

        out
    }
}

impl<T> InnerVec<T> {
    fn new() -> Self {
        let first_block = Block::<T>::new();

        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            cap: AtomicUsize::new(BLOCK_CAP),
            buf: first_block,
            buf_tail: AtomicPtr::new(first_block),
            state: AtomicUsize::new(AVAILABLE),
            ref_count: AtomicUsize::new(1),
            read_cache: Position {
                pos: AtomicUsize::new(0),
                ptr: AtomicPtr::new(first_block),
            },
            write_cache: Position {
                pos: AtomicUsize::new(0),
                ptr: AtomicPtr::new(first_block),
            },
        }
    }

    /// Lock-free capacity expansion
    #[cold]
    fn ensure_capacity(&self, needed: usize) {
        let current_cap = self.cap.load(Ordering::Acquire);
        if needed <= current_cap {
            return;
        }

        // Calculate blocks needed
        let blocks_to_add = ((needed - current_cap) + BLOCK_CAP_MASK) >> BLOCK_SHIFT;

        // Lock-free allocation
        if self.state.compare_exchange(
            AVAILABLE,
            BUSY,
            Ordering::Acquire,
            Ordering::Relaxed
        ).is_ok() {
            // Double-check after acquiring lock
            if needed > self.cap.load(Ordering::Relaxed) {
                for _ in 0..blocks_to_add {
                    self.add_block_cold();
                }
            }
            self.state.store(AVAILABLE, Ordering::Release);
        } else {
            // Wait for allocation to complete
            let backoff = Backoff::new();
            while self.cap.load(Ordering::Acquire) < needed {
                backoff.snooze();
            }
        }
    }

    /// Cold path block allocation
    #[cold]
    fn add_block_cold(&self) {
        let new_block = Block::new();
        let old_tail = self.buf_tail.swap(new_block, Ordering::AcqRel);

        unsafe {
            (*old_tail).next.store(new_block, Ordering::Release);
        }

        self.cap.fetch_add(BLOCK_CAP, Ordering::AcqRel);
    }

    /// Optimized block traversal with caching
    #[inline]
    fn get_block_fast(&self, block_idx: usize, cache: &Position<T>) -> *mut Block<T> {
        // Try cache first
        let cached_idx = cache.pos.load(Ordering::Relaxed);
        let cached_ptr = cache.ptr.load(Ordering::Relaxed);

        if cached_idx == block_idx && !cached_ptr.is_null() {
            return cached_ptr;
        }

        // Cache miss - traverse from closest point
        let (start_idx, mut block) = if cached_idx < block_idx && !cached_ptr.is_null() {
            (cached_idx, cached_ptr)
        } else {
            (0, self.buf)
        };

        // Fast traversal
        for _ in start_idx..block_idx {
            unsafe {
                let next = (*block).next.load(Ordering::Acquire);
                if next.is_null() {
                    break;
                }
                block = next;
            }
        }

        // Update cache
        cache.pos.store(block_idx, Ordering::Relaxed);
        cache.ptr.store(block, Ordering::Relaxed);

        block
    }

    #[inline(always)]
    unsafe fn get_slot_fast(&self, pos: usize) -> &Slot<T> {
        let block = self.get_block_fast(block_index(pos), &self.read_cache);
        &(*block).slots[index_in_block(pos)]
    }

    #[inline(always)]
    unsafe fn get_write_slot_fast(&self, pos: usize) -> &Slot<T> {
        let block = self.get_block_fast(block_index(pos), &self.write_cache);
        &(*block).slots[index_in_block(pos)]
    }

    #[inline(always)]
    unsafe fn get_read_slot_fast(&self, pos: usize) -> &Slot<T> {
        let block = self.get_block_fast(block_index(pos), &self.read_cache);
        &(*block).slots[index_in_block(pos)]
    }

    #[inline(always)]
    unsafe fn get_write_slot_unchecked(&self, pos: usize) -> &Slot<T> {
        let block_idx = block_index(pos);
        let mut block = self.buf;
        for _ in 0..block_idx {
            block = (*block).next.load(Ordering::Relaxed);
        }
        &(*block).slots[index_in_block(pos)]
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicVec")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl<T> Clone for AtomicVec<T> {
    fn clone(&self) -> Self {
        let inner = self.inner();
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        Self { inner: self.inner }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*(self.inner as *mut InnerVec<T>) };

        if inner.ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            fence(Ordering::Acquire);

            let head = inner.head.load(Ordering::Relaxed);
            let len = inner.len.load(Ordering::Relaxed);
            let cap = inner.cap.load(Ordering::Relaxed);

            // Drop values if necessary
            if mem::needs_drop::<T>() && len > 0 {
                unsafe {
                    for i in 0..len {
                        let pos = (head + i) & (cap - 1);
                        inner.get_read_slot_fast(pos).read_unchecked();
                    }
                }
            }

            // Deallocate blocks
            unsafe {
                let mut block = inner.buf;
                while !block.is_null() {
                    let next = (*block).next.load(Ordering::Relaxed);
                    Block::dealloc(block);
                    block = next;
                }

                // Drop inner
                drop(Box::from_raw(self.inner as *mut InnerVec<T>));
            }
        }
    }
}

impl<T> FromIterator<T> for AtomicVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let vec = Self::with_capacity(lower.max(BLOCK_CAP));

        for item in iter {
            vec.push(item);
        }
        vec
    }
}

impl<T> Default for AtomicVec<T> {
    fn default() -> Self {
        Self::new()
    }
}