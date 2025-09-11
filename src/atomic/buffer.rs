use crossbeam_utils::CachePadded;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub struct AtomicBuffer<T> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: Box<[CachePadded<AtomicPtr<T>>]>,
    cap: usize,
    cap_mask: usize,
}

impl<T> AtomicBuffer<T> {
    /// Standard constructor with default capacity of 32
    pub fn new() -> Self {
        Self::with_capacity(32)
    }

    /// Constructor with specified capacity (must be a power of 2)
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two(), "capacity must be power of two");

        let mut slots = Vec::with_capacity(cap);
        for _ in 0..cap {
            // Initialize each slot as a null pointer, wrapped in CachePadded
            slots.push(CachePadded::new(AtomicPtr::new(ptr::null_mut())));
        }

        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            slots: slots.into_boxed_slice(),
            cap,
            cap_mask: cap - 1, // used for fast modulo (index wrapping)
        }
    }

    /// Attempt to push a pointer into the buffer
    #[inline]
    pub fn push(&self, ptr: *mut T) -> Result<(), *mut T> {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);

        // Buffer full: cannot push more elements
        if tail.wrapping_sub(head) == self.cap {
            return Err(ptr);
        }

        let idx = tail & self.cap_mask; // calculate slot index using bitmask
        let slot = &self.slots[idx];

        // Write only if slot is empty (CAS prevents overwrite)
        let prev =
            slot.compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed);

        if prev.is_ok() {
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            Ok(())
        } else {
            // Someone else wrote into this slot concurrently
            Err(ptr)
        }
    }

    /// Attempt to pop a pointer from the buffer
    #[inline]
    pub fn pop(&self) -> Option<*mut T> {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);

        // Buffer empty
        if head == tail {
            return None;
        }

        let idx = head & self.cap_mask; // calculate slot index using bitmask
        let slot = &self.slots[idx];

        // Swap the slot value with null; atomic operation ensures no races
        let val = slot.swap(ptr::null_mut(), Ordering::Acquire);

        if !val.is_null() {
            self.head.store(head.wrapping_add(1), Ordering::Release);
            Some(val)
        } else {
            // Slot was unexpectedly empty (contention)
            None
        }
    }

    /// Returns the capacity of the buffer
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }
}
