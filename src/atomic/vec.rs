use crate::atomic::AtomicBuffer;
use crate::mutex::{Backoff, Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

// Block constants for internal allocation
const BLOCK_CAP: usize = 32; // number of slots per block
const BLOCK_CAP_MASK: usize = BLOCK_CAP - 1;
const BLOCK_SHIFT: u32 = 5; // log2(BLOCK_CAP)

// Index metadata flags
const INDEX_SHIFT: usize = 1; // lower bit reserved for flags
const HAS_NEXT: usize = 1; // indicates next block exists

// Slot state flags
const WRITE: usize = 1; // slot has been written
const READ: usize = 2; // slot has been read

/// Represents a single slot in a block.
/// UnsafeCell allows interior mutability without borrowing restrictions.
/// The state is atomic to allow concurrent access.
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicUsize,
}

impl<T> Slot<T> {
    #[inline(always)]
    fn wait_write(&self) {
        let backoff = Backoff::new();
        // Wait until the slot is written by a producer
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.snooze();
        }
    }
}

/// Represents a block of slots.
/// Each block has a next pointer for linked allocation and a read counter
/// to determine when it can be recycled.
struct Block<T> {
    next: CachePadded<AtomicPtr<Block<T>>>, // pointer to the next block
    read_count: CachePadded<AtomicUsize>,   // number of successfully read slots
    slots: [Slot<T>; BLOCK_CAP],
}

impl<T> Block<T> {
    const LAYOUT: Layout = Layout::new::<Self>();

    /// Allocates a zeroed block in heap memory.
    #[inline]
    fn new() -> Box<Self> {
        let ptr = unsafe { alloc_zeroed(Self::LAYOUT) };
        if ptr.is_null() {
            handle_alloc_error(Self::LAYOUT)
        }
        unsafe { Box::from_raw(ptr.cast()) }
    }

    /// Resets the block metadata for reuse
    #[inline]
    fn reset(&mut self) {
        self.next.store(ptr::null_mut(), Ordering::Relaxed);
        self.read_count.store(0, Ordering::Relaxed);
        for slot in &self.slots {
            slot.state.store(0, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    fn wait_next(&self) -> *mut Self {
        let backoff = Backoff::new();
        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            backoff.snooze();
        }
    }
}

/// Cache of block pointers for fast access by index.
/// Uses index tagging to avoid contention.
struct BlockArray<T> {
    entries: [CachePadded<BlockEntry<T>>; BLOCK_CAP],
}

struct BlockEntry<T> {
    index: AtomicUsize,       // logical index of block
    ptr: AtomicPtr<Block<T>>, // pointer to the block
}

impl<T> BlockArray<T> {
    fn new() -> Self {
        const INIT_ENTRY: CachePadded<BlockEntry<()>> = CachePadded::new(BlockEntry {
            index: AtomicUsize::new(usize::MAX), // invalid
            ptr: AtomicPtr::new(ptr::null_mut()),
        });

        let entries: [CachePadded<BlockEntry<()>>; BLOCK_CAP] = [INIT_ENTRY; BLOCK_CAP];

        unsafe {
            Self {
                entries: mem::transmute(entries),
            }
        }
    }

    #[inline(always)]
    fn get(&self, idx: usize) -> *mut Block<T> {
        let slot = &self.entries[idx & BLOCK_CAP_MASK];
        if slot.index.load(Ordering::Acquire) == idx {
            slot.ptr.load(Ordering::Acquire)
        } else {
            ptr::null_mut()
        }
    }

    #[inline(always)]
    fn set(&self, idx: usize, block: *mut Block<T>) {
        let slot = &self.entries[idx & BLOCK_CAP_MASK];
        // write pointer first, then index to avoid race where index is seen but pointer is not ready
        slot.ptr.store(block, Ordering::Release);
        slot.index.store(idx, Ordering::Release);
    }

    #[inline(always)]
    fn reset(&self, idx: usize) {
        self.set(idx, ptr::null_mut());
    }
}

/// Represents a position (head/tail) in the vector.
struct Position<T> {
    index: AtomicUsize,
    block: AtomicPtr<Block<T>>,
}

/// The internal representation of the vector.
#[repr(C)]
struct InnerVec<T> {
    head: CachePadded<Position<T>>,
    tail: CachePadded<Position<T>>,
    len: CachePadded<AtomicUsize>,
    block_array: BlockArray<T>,
    free_list: AtomicBuffer<Block<T>>, // free list of reusable blocks
    ref_count: CachePadded<AtomicUsize>, // reference count for cloning
    lock: CachePadded<AtomicUsize>,    // shared/exclusive lock
}

/// Thread-safe vector with atomic operations.
#[repr(transparent)]
pub struct AtomicVec<T> {
    inner: *const InnerVec<T>,
}

unsafe impl<T: Send> Send for AtomicVec<T> {}
unsafe impl<T: Send> Sync for AtomicVec<T> {}

#[allow(unsafe_op_in_unsafe_fn)]
impl<T> AtomicVec<T> {
    /// Creates a new empty atomic vector.
    #[inline]
    pub fn new() -> Self {
        let inner = InnerVec {
            head: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            len: CachePadded::new(AtomicUsize::new(0)),
            block_array: BlockArray::new(),
            free_list: AtomicBuffer::with_capacity(64),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
            lock: CachePadded::new(AtomicUsize::new(0)),
        };

        // Allocate and assign the first block
        let new = Box::into_raw(Block::<T>::new());
        inner.tail.block.store(new, Ordering::Release);
        inner.head.block.store(new, Ordering::Release);
        inner.block_array.set(0, new);

        Self {
            inner: Box::into_raw(Box::new(inner)),
        }
    }

    /// Initializes the vector with a given capacity using a provided initializer.
    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::new();
        for _ in 0..cap {
            vec.push(initializer());
        }
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
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        let inner = self.inner();
        let head = inner.head.index.load(Ordering::Acquire) >> INDEX_SHIFT;
        let tail = inner.tail.index.load(Ordering::Acquire) >> INDEX_SHIFT;

        ((tail - head) & !BLOCK_CAP_MASK) + BLOCK_CAP
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
    pub unsafe fn push_unchecked(&self, value: T) {
        let inner = self.inner();
        let backoff = Backoff::new();
        let mut tail = inner.tail.index.load(Ordering::Acquire);
        let mut block = inner.tail.block.load(Ordering::Acquire);

        loop {
            let offset = (tail >> INDEX_SHIFT) & BLOCK_CAP_MASK;

            if offset == BLOCK_CAP - 1 {
                // If tail reached block end, reload tail and block
                backoff.snooze();
                tail = inner.tail.index.load(Ordering::Acquire);
                block = inner.tail.block.load(Ordering::Acquire);
                continue;
            }

            let new_tail = tail + (1 << INDEX_SHIFT);

            // Atomically claim the next slot
            match inner.tail.index.compare_exchange(
                tail,
                new_tail,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Preallocate next block if this is the last slot in current block
                    if offset + 1 == BLOCK_CAP - 1 {
                        let next = inner
                            .free_list
                            .pop()
                            .unwrap_or_else(|| Box::into_raw(Block::<T>::new()));
                        let next_index = new_tail.wrapping_add(1 << INDEX_SHIFT);
                        let block_idx = (new_tail >> INDEX_SHIFT) >> BLOCK_SHIFT;

                        inner.tail.block.store(next, Ordering::Release);
                        inner.tail.index.store(next_index, Ordering::Release);
                        (*block).next.store(next, Ordering::Release);
                        inner.block_array.set(block_idx + 1, next);
                    }

                    // Write the value to the slot
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.value.get().write(MaybeUninit::new(value));
                    // Publish the WRITE state to signal readers
                    slot.state.store(WRITE, Ordering::Release);

                    // Increment the vector length
                    inner.len.fetch_add(1, Ordering::Release);
                    return;
                }
                Err(t) => {
                    // CAS failed, reload tail and block and retry
                    tail = t;
                    block = inner.tail.block.load(Ordering::Acquire);
                    backoff.snooze();
                }
            }
        }
    }

    /// Thread-safe push with shared lock.
    pub fn push(&self, value: T) {
        self.wait_lock_shared();
        unsafe {
            self.push_unchecked(value);
        }
        self.release_lock_shared();
    }

    /// Pop a value without acquiring the shared lock.
    /// Unsafe because concurrent access may occur if called externally.
    pub unsafe fn pop_unchecked(&self) -> Option<T> {
        let inner = self.inner();

        if inner.len.load(Ordering::Acquire) == 0 {
            return None;
        }

        let backoff = Backoff::new();
        let mut head = inner.head.index.load(Ordering::Acquire);
        let mut block = inner.head.block.load(Ordering::Acquire);

        loop {
            let offset = (head >> INDEX_SHIFT) & BLOCK_CAP_MASK;

            if offset == BLOCK_CAP - 1 || block.is_null() {
                // End of block or null block, reload head and block
                backoff.snooze();
                head = inner.head.index.load(Ordering::Acquire);
                block = inner.head.block.load(Ordering::Acquire);
                continue;
            }

            let mut new_head = head + (1 << INDEX_SHIFT);

            // Update HAS_NEXT flag if moving to next block
            if new_head & HAS_NEXT == 0 {
                fence(Ordering::SeqCst);
                let tail = inner.tail.index.load(Ordering::Relaxed);
                let head_idx = head >> INDEX_SHIFT;
                let tail_idx = tail >> INDEX_SHIFT;

                if head_idx == tail_idx {
                    return None; // Vector is empty
                }

                if (head_idx >> BLOCK_SHIFT) != (tail_idx >> BLOCK_SHIFT) {
                    new_head |= HAS_NEXT;
                }
            }

            // Atomically advance the head
            match inner.head.index.compare_exchange(
                head,
                new_head,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Advance to next block if needed
                    if offset + 1 == BLOCK_CAP - 1 {
                        let next = (*block).wait_next();
                        let mut next_index = (new_head & !HAS_NEXT).wrapping_add(1 << INDEX_SHIFT);

                        if !(*next).next.load(Ordering::Relaxed).is_null() {
                            next_index |= HAS_NEXT;
                        }

                        inner.head.block.store(next, Ordering::Release);
                        inner.head.index.store(next_index, Ordering::Release);
                    }

                    // Read the value from the slot
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.wait_write(); // wait until the slot is written
                    let value = slot.value.get().read().assume_init();
                    slot.state.store(READ, Ordering::Release);

                    // Update the vector length
                    inner.len.fetch_sub(1, Ordering::Release);

                    // Recycle block if all slots read
                    if (*block).read_count.fetch_add(1, Ordering::AcqRel) + 1 == BLOCK_CAP - 1 {
                        let block_idx = (head >> INDEX_SHIFT) >> BLOCK_SHIFT;
                        inner.block_array.reset(block_idx);
                        (*block).reset();
                        if let Err(e) = inner.free_list.push(block) {
                            drop(Box::from_raw(e));
                        }
                    }

                    return Some(value);
                }
                Err(h) => {
                    // CAS failed, reload head and block
                    head = h;
                    block = inner.head.block.load(Ordering::Acquire);
                    backoff.spin();
                }
            }
        }
    }

    /// Thread-safe pop with shared lock.
    pub fn pop(&self) -> Option<T> {
        self.wait_lock_shared();
        let val = unsafe { self.pop_unchecked() };
        self.release_lock_shared();
        val
    }

    /// Reset the vector to a new capacity using a provided initializer.
    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> usize {
        let inner = self.inner();
        self.wait_lock_exclusive();

        inner.head.index.store(0, Ordering::Relaxed);
        inner.tail.index.store(0, Ordering::Relaxed);
        inner.len.store(0, Ordering::Relaxed);

        for i in 0..BLOCK_CAP {
            inner.block_array.set(i, ptr::null_mut());
        }

        unsafe {
            for _ in 0..new_cap {
                self.push_unchecked(initializer());
            }
        }

        self.release_lock_exclusive();
        new_cap
    }

    /// Convert the atomic vector into a standard Vec<T>.
    /// Consumes all elements, thread-safely.
    pub fn as_vec(&self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.len());
        if self.is_empty() {
            return out;
        }

        self.wait_lock_exclusive();
        unsafe {
            while let Some(value) = self.pop_unchecked() {
                out.push(value);
            }
        }
        self.release_lock_exclusive();
        out
    }
}

impl<T> Clone for AtomicVec<T> {
    /// Cloning the AtomicVec only increases the reference count.
    /// The underlying data is shared safely between clones.
    fn clone(&self) -> Self {
        let inner = self.inner();
        inner.ref_count.fetch_add(1, Ordering::Relaxed); // increment ref count
        Self { inner: self.inner }
    }
}

impl<T> Drop for AtomicVec<T> {
    /// Drops the AtomicVec.
    /// If this is the last reference, deallocates all blocks and inner structures.
    fn drop(&mut self) {
        let inner = unsafe { &*self.inner };

        // Only deallocate if this is the last reference
        if inner.ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire); // synchronize with other threads

        unsafe {
            // 1. Release all blocks from head to tail
            let mut block = inner.head.block.load(Ordering::Relaxed);

            while !block.is_null() {
                let next = (*block).next.load(Ordering::Relaxed);

                // Drop all written elements in the block if T needs drop
                if mem::needs_drop::<T>() {
                    for slot in &(*block).slots {
                        if slot.state.load(Ordering::Relaxed) & WRITE != 0 {
                            ptr::drop_in_place(slot.value.get());
                        }
                    }
                }

                drop(Box::from_raw(block)); // deallocate block
                block = next;
            }

            // 2. Release blocks in the free list
            while let Some(b) = inner.free_list.pop() {
                drop(Box::from_raw(b));
            }

            // 3. Deallocate InnerVec itself
            drop(Box::from_raw(self.inner as *mut InnerVec<T>));
        }
    }
}

impl<T> FromIterator<T> for AtomicVec<T> {
    /// Creates an AtomicVec from an iterator of items.
    /// This method pushes items one by one.
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let vec = Self::new();
        for item in iter {
            vec.push(item);
        }
        vec
    }
}

impl<T> Default for AtomicVec<T> {
    /// Creates a new empty AtomicVec
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicVec<T> {
    /// Implements Debug for AtomicVec.
    /// Displays the current length and capacity.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicVec")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}
