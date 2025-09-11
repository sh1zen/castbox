use crate::collections::AtomicVec;
use crate::core::scondvar::SCondVar;
use crate::core::smutex::SMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct MpmcInner<T> {
    buffer: AtomicVec<T>,
    mutex: SMutex,
    condvar: SCondVar,
    ref_count: AtomicUsize,
    bounded: usize,
    closed: AtomicBool,
}

pub struct Mpmc<T> {
    ptr: *const MpmcInner<T>,
}

unsafe impl<T: Send> Send for Mpmc<T> {}
unsafe impl<T: Sync> Sync for Mpmc<T> {}

impl<T> Mpmc<T> {
    /// create a new unbounded channel
    pub fn new() -> Mpmc<T> {
        let ptr = Box::into_raw(Box::new(MpmcInner {
            buffer: AtomicVec::new(),
            mutex: SMutex::new(),
            condvar: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
            bounded: 0,
            closed: AtomicBool::new(false),
        }));

        if ptr.is_null() {
            panic!("Invalid allocation for Mpmc");
        }

        Self { ptr }
    }

    /// create a new bounded channel
    pub fn bounded(n: usize) -> Mpmc<T> {
        let c = Self::new();
        c.inner_mut().bounded = n;
        c
    }

    #[inline(always)]
    fn inner(&self) -> &MpmcInner<T> {
        unsafe { &*self.ptr }
    }

    #[inline(always)]
    fn inner_mut(&self) -> &mut MpmcInner<T> {
        let ptr = self.ptr as *mut MpmcInner<T>;
        unsafe { &mut *ptr }
    }

    /// non-blocking send
    pub fn send(&self, value: T) -> Result<(), T> {
        let inner = self.inner();

        if inner.closed.load(Ordering::Acquire) {
            return Err(value);
        }

        // controlla il limite
        if inner.bounded > 0 && inner.buffer.len() >= inner.bounded {
            return Err(value);
        }

        let _guard = inner.mutex.lock();
        inner.buffer.push(value);

        inner.condvar.notify_one();
        Ok(())
    }

    /// non-blocking receive
    pub fn try_recv(&self) -> Option<T> {
        let inner = self.inner();
        let _guard = inner.mutex.lock();
        inner.buffer.pop()
    }

    /// blocking receive
    pub fn recv(&self) -> Option<T> {
        let inner = self.inner();

        loop {
            let guard = inner.mutex.lock();

            if let Some(msg) = inner.buffer.pop() {
                return Some(msg);
            }

            if inner.closed.load(Ordering::Acquire) {
                return None;
            }

            let _ = inner.condvar.wait(guard);
        }
    }

    /// Chiude il canale
    pub fn close(&self) {
        let inner = self.inner();
        let _guard = inner.mutex.lock();
        inner.closed.store(true, Ordering::Release);
        inner.condvar.notify_all();
    }
}

impl<T> Clone for Mpmc<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for Mpmc<T> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            let ptr = self.ptr as *mut MpmcInner<T>;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}
