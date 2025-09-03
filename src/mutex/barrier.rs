use crate::mutex::Mutex;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

struct BarrierInner {
    ref_count: AtomicUsize,
    lock: Mutex,
}

#[repr(transparent)]
pub struct Barrier {
    ptr: *const BarrierInner,
}

impl Barrier {
    pub fn new() -> Barrier {
        let m = Mutex::new();
        m.lock_exclusive();

        let ptr = Box::into_raw(Box::new(BarrierInner {
            ref_count: AtomicUsize::new(1),
            lock: m,
        }));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Barrier");
        }
        Self { ptr }
    }

    pub fn count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    #[inline(always)]
    fn inner(&self) -> &BarrierInner {
        unsafe { &*self.ptr }
    }

    pub fn wait(&self) {
        self.inner().lock.lock_group();
    }

    pub fn release(&self) {
        self.inner().lock.unlock_exclusive();
        self.inner().lock.unlock_all_group();
    }
}

impl Clone for Barrier {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Relaxed);
        Barrier { ptr: self.ptr }
    }
}

impl Drop for Barrier {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);

            let ptr = self.ptr as *mut BarrierInner;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}
