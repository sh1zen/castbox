use crate::mutex::Backoff;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SpinLockCell<T> {
    state: AtomicUsize,
    val: UnsafeCell<T>,
}

const UPDATING: usize = 0;
const AVAILABLE: usize = 1;

impl<T> SpinLockCell<T> {
    pub fn new(val: T) -> SpinLockCell<T> {
        SpinLockCell {
            val: UnsafeCell::new(val),
            state: AtomicUsize::new(AVAILABLE),
        }
    }

    #[inline]
    pub fn get_ref(&self) -> &T {
        unsafe { &*self.val.get() }
    }

    #[inline]
    pub fn get_mut_ref(&self) -> &mut T {
        unsafe { &mut *self.val.get() }
    }

    #[inline]
    pub fn lock(&self) {
        let backoff = Backoff::new();
        while self
            .state
            .compare_exchange(AVAILABLE, UPDATING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            backoff.snooze();
        }
    }

    #[inline]
    pub fn release(&self) {
        self.state.store(AVAILABLE, Ordering::Release);
    }
}
