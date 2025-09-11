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
    /// Costruttore standard con capacità 64
    pub fn new() -> Self {
        Self::with_capacity(32)
    }

    /// Costruttore con capacità specificata (deve essere potenza di 2)
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two(), "capacity must be power of two");

        let mut slots = Vec::with_capacity(cap);
        for _ in 0..cap {
            slots.push(CachePadded::new(AtomicPtr::new(ptr::null_mut())));
        }

        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            slots: slots.into_boxed_slice(),
            cap,
            cap_mask: cap - 1,
        }
    }

    #[inline]
    pub fn push(&self, ptr: *mut T) -> Result<(), *mut T> {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);

        // buffer pieno
        if tail.wrapping_sub(head) == self.cap {
            return Err(ptr);
        }

        let idx = tail & self.cap_mask;
        let slot = &self.slots[idx];

        // scrivi solo se è vuoto (CAS evita overwrite)
        let prev =
            slot.compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed);

        if prev.is_ok() {
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            Ok(())
        } else {
            Err(ptr) // qualcuno ha scritto nello slot (contenzione)
        }
    }

    #[inline]
    pub fn pop(&self) -> Option<*mut T> {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);

        if head == tail {
            return None; // vuoto
        }

        let idx = head & self.cap_mask;
        let slot = &self.slots[idx];

        // CAS: prendi il valore solo se non è null
        let val = slot.swap(ptr::null_mut(), Ordering::Acquire);

        if !val.is_null() {
            self.head.store(head.wrapping_add(1), Ordering::Release);
            Some(val)
        } else {
            None
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }
}
