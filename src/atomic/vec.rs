use crate::mutex::{Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::{self, MaybeUninit};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

/// Block capacity - same as original
const BLOCK_CAP: usize = 32;

/// Simple slot - exactly like original but optimized
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    // lock: Mutex,
}

impl<T> Slot<T> {
    #[inline]
    fn write(&self, value: T) {
        unsafe {
            (*self.value.get()).write(value);
        }
    }

    #[inline]
    fn read(&self) -> T {
        unsafe { (*self.value.get()).assume_init_read() }
    }

    #[inline]
    fn get_ref(&self) -> &T {
        unsafe { (*self.value.get()).assume_init_ref() }
    }
}

/// Block exactly like original
struct Block<T> {
    next: AtomicPtr<Block<T>>,
    slots: [Slot<T>; BLOCK_CAP],
}

#[inline(always)]
fn block_index(pos: usize) -> usize {
    pos >> 5
}

#[inline(always)]
fn index_in_block(pos: usize) -> usize {
    pos & (BLOCK_CAP - 1)
}

impl<T> Block<T> {
    const LAYOUT: Layout = {
        let layout = Layout::new::<Self>();
        assert!(
            layout.size() != 0,
            "Block should never be zero-sized, as it has an AtomicPtr field"
        );
        layout
    };

    fn new() -> Box<Self> {
        let ptr = unsafe { alloc_zeroed(Self::LAYOUT) };
        if ptr.is_null() {
            handle_alloc_error(Self::LAYOUT)
        }
        unsafe { Box::from_raw(ptr.cast()) }
    }
}

struct Position<T> {
    pos: AtomicUsize,
    ptr: AtomicPtr<Block<T>>,
}

/// Inner structure - simplified from original
struct InnerVec<T> {
    buf: Box<Block<T>>,
    buf_tail: CachePadded<AtomicPtr<Block<T>>>,
    c: CachePadded<Position<T>>,
    head: CachePadded<AtomicUsize>,
    len: CachePadded<AtomicUsize>,
    cap: CachePadded<AtomicUsize>,
    ref_count: CachePadded<AtomicUsize>,
}

/// AtomicVec - back to basics but optimized
#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: CachePadded<*const InnerVec<T>>,
}

unsafe impl<T: Send> Send for AtomicVec<T> {}
unsafe impl<T: Send> Sync for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerVec::new()));
        Self {
            ptr: CachePadded::new(ptr),
        }
    }

    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::new();
        let inner = vec.inner();

        // Simple sequential fill - no locks needed
        for i in 0..cap {
            inner.maybe_add_block();
            inner.get_slot(i).write(initializer());
        }

        inner.set_head(0);
        inner.set_len(cap);
        vec
    }

    #[inline(always)]
    fn inner(&self) -> &InnerVec<T> {
        unsafe { &**self.ptr }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner().get_len()
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.inner().get_cap()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Push - minimal overhead version
    pub fn push(&self, value: T) {
        let inner = self.inner();

        inner.maybe_add_block();

        // Single atomic read for length
        let len = inner.len.fetch_add(1, Ordering::AcqRel);
        let head = inner.get_head();

        // Write directly - no additional synchronization needed
        let write_pos = (head + len) % inner.get_cap();

        inner.get_slot(write_pos).write(value);
    }

    /// Pop - minimal overhead version
    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();

        // Try to decrement length atomically
        let mut current_len = inner.get_len();
        loop {
            if current_len == 0 {
                return None;
            }

            match inner.len.compare_exchange_weak(
                current_len,
                current_len - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(x) => current_len = x,
            }
        }

        // Read the value
        let head = inner.get_head();
        let value = inner.get_slot(head).read();

        // Update head
        if current_len == 1 {
            inner.set_head(0); // Reset when empty
        } else {
            inner.set_head((head + 1) % inner.get_cap());
        }

        Some(value)
    }

    /// Get with minimal guard overhead
    pub fn get(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        let inner = self.inner();

        if index >= inner.get_len() {
            return None;
        }

        let head = inner.get_head();
        let pos = (head + index) % inner.get_cap();

        Some(WatchGuardRef::new(
            inner.get_slot(pos).get_ref(),
            Mutex::new(),
        ))
    }

    /// Reset - fast version
    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> usize {
        let inner = self.inner();

        // Reset counters
        inner.len.store(0, Ordering::Release);
        inner.set_head(0);

        // Fill with new values
        for i in 0..new_cap {
            inner.maybe_add_block();
            inner.get_slot(i).write(initializer());
        }

        inner.len.store(new_cap, Ordering::Release);
        new_cap
    }

    /// as_vec - fast drain
    pub fn as_vec(&self) -> Vec<T> {
        let inner = self.inner();
        let head = inner.get_head();
        let len = inner.get_len();
        let cap = inner.get_cap();

        let mut out = Vec::with_capacity(len);

        for i in 0..len {
            let pos = (head + i) % cap;
            out.push(inner.get_slot(pos).read());
        }

        // Reset to empty
        inner.set_head(0);
        inner.len.store(0, Ordering::Release);
        out
    }
}

impl<T> InnerVec<T> {
    fn new() -> Self {
        let b = Block::<T>::new();
        let ptr = Box::into_raw(b);

        Self {
            buf: unsafe { Box::from_raw(ptr) },
            buf_tail: CachePadded::new(AtomicPtr::new(ptr)),
            head: CachePadded::new(AtomicUsize::new(0)),
            c: CachePadded::new(Position {
                pos: AtomicUsize::new(0),
                ptr: AtomicPtr::new(ptr),
            }),
            len: CachePadded::new(AtomicUsize::new(0)),
            cap: CachePadded::new(AtomicUsize::new(BLOCK_CAP)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        }
    }

    #[inline(always)]
    fn get_head(&self) -> usize {
        self.head.load(Ordering::Acquire)
    }

    #[inline(always)]
    fn set_head(&self, v: usize) {
        self.head.store(v, Ordering::Release)
    }

    #[inline(always)]
    fn get_len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    #[inline(always)]
    fn set_len(&self, v: usize) {
        self.len.store(v, Ordering::Release);
    }

    #[inline(always)]
    fn get_cap(&self) -> usize {
        self.cap.load(Ordering::Acquire)
    }

    /// Simplified block addition
    fn maybe_add_block(&self) {
        if self.get_len() < self.get_cap() {
            return;
        }

        let new_block = Box::into_raw(Block::new());

        let block = self.buf_tail.swap(new_block, Ordering::Acquire);

        self.cap.fetch_add(BLOCK_CAP, Ordering::Release);

        if block.is_null() {
            panic!("must not be null")
        }

        let block = unsafe { &*block };

        block.next.store(new_block, Ordering::Release);
    }

    //#[inline(always)]
    fn get_block(&self, n: usize) -> &Block<T> {
        // cache attuale
        let mut block = &*self.buf;
        let mut start = 0;

        let cached_ptr = self.c.ptr.load(Ordering::Acquire);
        if !cached_ptr.is_null() {
            let cached_pos = self.c.pos.load(Ordering::Acquire);

            if cached_pos == n {
                // cache hit perfetto
                return unsafe { &*cached_ptr };
            } else if cached_pos < n {
                // possiamo partire dal blocco già salvato
                start = cached_pos;
                block = unsafe { &*cached_ptr };
            }
        }

        // avanza solo quanto serve
        for _ in start..n {
            let next = block.next.load(Ordering::Acquire);
            if next.is_null() {
                // non ancora allocato: nessun blocco n disponibile
                return block; // oppure return None
            }
            block = unsafe { &*next };
        }

        // aggiorna cache con il blocco esatto trovato
        self.c.ptr.store(block as *const _ as *mut _, Ordering::Release);
        self.c.pos.store(n, Ordering::Release);

        block
    }

    #[inline(always)]
    fn get_slot(&self, pos: usize) -> &Slot<T> {
        &self.get_block(block_index(pos)).slots[index_in_block(pos)]
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
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        let inner = self.inner();
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);

            unsafe {
                let boxed: Box<InnerVec<T>> = Box::from_raw(*self.ptr as *mut InnerVec<T>);

                let head = boxed.get_head();
                let len = boxed.get_len();
                let cap = boxed.get_cap();

                // Drop elements
                if mem::needs_drop::<T>() && len > 0 {
                    for i in 0..len {
                        let idx = (head + i) % cap;
                        let _ = boxed.get_slot(idx).read(); // This drops the value
                    }
                }

                // Deallocate blocks
                let mut block_ptr: *mut Block<T> = Box::into_raw(boxed.buf);
                while !block_ptr.is_null() {
                    let next = (*block_ptr).next.load(Ordering::Acquire);
                    drop(Box::from_raw(block_ptr));
                    block_ptr = next;
                }
            }
        }
    }
}

impl<T> FromIterator<T> for AtomicVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let v = AtomicVec::new();
        for item in iter {
            v.push(item);
        }
        v
    }
}

impl<T> Default for AtomicVec<T> {
    fn default() -> Self {
        Self::new()
    }
}
