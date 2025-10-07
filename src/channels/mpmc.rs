use crate::atomic::AtomicVec;
use crate::core::scondvar::SCondVar;
use crate::core::smutex::SMutex;
use std::sync::atomic;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Internal shared state for the MPMC channel
struct MpmcInner<T> {
    // shared message buffer
    buffer: AtomicVec<T>,
    // mutual exclusion for push/pop operations
    mutex: SMutex,
    // used for blocking receives
    condvar: SCondVar,
    // reference counter for cloning
    ref_count: AtomicUsize,
    // 0 = unbounded, otherwise max queue size
    bounded: usize,
    // channel closed flag
    closed: AtomicBool,
}

/// Multi-producer, multi-consumer channel.
/// Internally reference-counted and lock-based for safety.
pub struct Mpmc<T> {
    ptr: *const MpmcInner<T>,
}

// SAFETY:
// - `Mpmc` is `Send` if T is `Send`
// - `Mpmc` is `Sync` if T is `Sync`
unsafe impl<T: Send> Send for Mpmc<T> {}
unsafe impl<T: Sync> Sync for Mpmc<T> {}

impl<T> Mpmc<T> {
    /// Creates a new **unbounded** MPMC channel
    pub fn new() -> Mpmc<T> {
        Self::bounded(0)
    }

    /// Creates a new **bounded** MPMC channel
    /// If `n == 0`, it behaves as unbounded
    pub fn bounded(n: usize) -> Mpmc<T> {
        let ptr = Box::into_raw(Box::new(MpmcInner {
            buffer: AtomicVec::new(),
            mutex: SMutex::new(),
            condvar: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
            bounded: n,
            closed: AtomicBool::new(false),
        }));

        if ptr.is_null() {
            panic!("Invalid allocation for Mpmc");
        }

        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &MpmcInner<T> {
        unsafe { &*self.ptr }
    }

    /// Attempts to send a value **without blocking**.
    ///
    /// Returns `Err(value)` if the buffer is full or the channel is closed.
    pub fn send(&self, value: T) -> Result<(), T> {
        let inner = self.inner();

        // Do not accept new messages if closed
        if inner.closed.load(Ordering::Acquire) {
            return Err(value);
        }

        // Check bounded capacity (if applicable)
        if inner.bounded > 0 && inner.buffer.len() >= inner.bounded {
            return Err(value);
        }

        // Acquire lock for safe access
        let _guard = inner.mutex.lock();
        inner.buffer.push(value);

        // Wake up one waiting receiver
        inner.condvar.notify_one();
        Ok(())
    }

    /// Attempts to receive a value **without blocking**.
    ///
    /// Returns `None` if the buffer is empty.
    pub fn try_recv(&self) -> Option<T> {
        let inner = self.inner();
        let _guard = inner.mutex.lock();
        inner.buffer.pop()
    }

    /// Receives a value, **blocking** until one is available or the channel is closed.
    pub fn recv(&self) -> Option<T> {
        let inner = self.inner();

        loop {
            let guard = inner.mutex.lock();

            // If buffer has an item, return it
            if let Some(msg) = inner.buffer.pop() {
                return Some(msg);
            }

            // If closed and empty, return None
            if inner.closed.load(Ordering::Acquire) {
                return None;
            }

            // Otherwise, wait for a sender to notify
            let _ = inner.condvar.wait(guard);
        }
    }

    /// Closes the channel.
    ///
    /// Any pending or future send attempts will fail,
    /// and all waiting receivers will be woken up.
    pub fn close(&self) {
        let inner = self.inner();
        let _guard = inner.mutex.lock();
        inner.closed.store(true, Ordering::Release);
        inner.condvar.notify_all();
    }
}

impl<T> Clone for Mpmc<T> {
    /// Clones the channel handle (increments refcount)
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for Mpmc<T> {
    fn drop(&mut self) {
        // Only free the shared state when the last clone is dropped
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            let ptr = self.ptr as *mut MpmcInner<T>;
            if !ptr.is_null() {
                unsafe { drop(Box::from_raw(ptr)) };
            }
        }
    }
}
