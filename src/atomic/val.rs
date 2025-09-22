use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use std::cell::UnsafeCell;
use std::mem;
use std::sync::atomic::{self, AtomicUsize, Ordering};

struct AtomicValInner<T> {
    val: UnsafeCell<T>,
    state: Mutex,
    ref_count: AtomicUsize,
}

impl<T> AtomicValInner<T> {
    fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(val),
            state: Mutex::new(),
            ref_count: AtomicUsize::new(1),
        }
    }
}

// Interior mutability: sicuro solo grazie al lock
unsafe impl<T: Send> Send for AtomicValInner<T> {}
unsafe impl<T: Sync> Sync for AtomicValInner<T> {}

#[repr(transparent)]
pub struct Atomic<T> {
    ptr: *const AtomicValInner<T>,
}

impl<T> Atomic<T> {
    pub fn new(val: T) -> Self {
        let inner = Box::new(AtomicValInner::new(val));
        let ptr = Box::into_raw(inner);
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicValInner<T> {
        unsafe { &*self.ptr }
    }

    pub fn get(&self) -> WatchGuardRef<'_, T> {
        let lock = &self.inner().state;
        lock.lock_shared();
        let val = unsafe { &*self.inner().val.get() }; // <-- da UnsafeCell
        WatchGuardRef::new(val, lock.clone())
    }

    pub fn get_mut(&self) -> WatchGuardMut<'_, T> {
        let lock = &self.inner().state;
        lock.lock_exclusive();
        let val = unsafe { &mut *self.inner().val.get() };
        WatchGuardMut::new(val, lock.clone())
    }
}

impl<T> Clone for Atomic<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Atomic<T> {
    /// Atomic store (scrittura atomica con lock esclusivo)
    pub fn store(&self, val: T) {
        let mut guard = self.get_mut();
        *guard = val;
    }

    /// Atomic swap (scambia il valore e ritorna il precedente)
    pub fn swap(&self, val: T) -> T {
        let mut guard = self.get_mut();
        mem::replace(&mut *guard, val)
    }
}

impl<T: PartialEq> Atomic<T> {
    /// Atomic compare_exchange (CAS): se `current` == contenuto, scrive `new`, altrimenti fallisce
    pub fn compare_exchange(&self, current: T, new: T) -> Result<T, WatchGuardMut<'_, T>> {
        let mut guard = self.get_mut();
        if *guard == current {
            let old = mem::replace(&mut *guard, new);
            Ok(old)
        } else {
            Err(guard)
        }
    }
}

impl<T> Drop for Atomic<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            unsafe { drop(Box::from_raw(self.ptr as *mut AtomicValInner<T>)) };
        }
    }
}
