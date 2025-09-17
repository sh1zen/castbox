use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use std::cell::UnsafeCell;
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
unsafe impl<T: Send + Sync> Sync for AtomicValInner<T> {}

#[repr(transparent)]
pub struct AtomicVal<T> {
    ptr: *const AtomicValInner<T>,
}

impl<T> AtomicVal<T> {
    pub fn new(val: T) -> Self {
        let inner = Box::new(AtomicValInner::new(val));
        let ptr = Box::into_raw(inner);
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicValInner<T> {
        unsafe { &*self.ptr }
    }

    pub fn read(&self) -> WatchGuardRef<'_, T> {
        let lock = &self.inner().state;
        lock.lock_shared();
        let val = unsafe { &*self.inner().val.get() }; // <-- da UnsafeCell
        WatchGuardRef::new(val, lock.clone())
    }

    pub fn write(&self) -> WatchGuardMut<'_, T> {
        let lock = &self.inner().state;
        lock.lock_exclusive();
        let val = unsafe { &mut *self.inner().val.get() }; // <-- da UnsafeCell
        WatchGuardMut::new(val, lock.clone())
    }
}

impl<T> Clone for AtomicVal<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVal<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            unsafe { drop(Box::from_raw(self.ptr as *mut AtomicValInner<T>)) };
        }
    }
}
