use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use std::cell::UnsafeCell;
use std::mem;
use std::mem::MaybeUninit;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::{self, AtomicUsize, Ordering};

struct Inner<T> {
    val: UnsafeCell<MaybeUninit<T>>,
    state: Mutex,
    ref_count: AtomicUsize,
}

impl<T> Inner<T> {
    fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(MaybeUninit::new(val)),
            state: Mutex::new(),
            ref_count: AtomicUsize::new(1),
        }
    }
}

// Interior mutability: sicuro solo grazie al lock
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Sync> Sync for Inner<T> {}

impl<T> UnwindSafe for Inner<T> {}
impl<T> RefUnwindSafe for Inner<T> {}

#[repr(transparent)]
pub struct AtomicCell<T> {
    ptr: *const Inner<T>,
}

impl<T> AtomicCell<T> {
    pub fn new(val: T) -> Self {
        let inner = Box::new(Inner::new(val));
        let ptr = Box::into_raw(inner);
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &Inner<T> {
        unsafe { &*self.ptr }
    }

    pub fn get(&self) -> WatchGuardRef<'_, T> {
        let lock = &self.inner().state;
        lock.lock_shared();
        let val = unsafe { (&*self.inner().val.get()).assume_init_ref() };
        WatchGuardRef::new(val, lock.clone())
    }

    pub fn get_mut(&self) -> WatchGuardMut<'_, T> {
        let lock = &self.inner().state;
        lock.lock_exclusive();
        let val = unsafe { (&mut *self.inner().val.get()).assume_init_mut() };
        WatchGuardMut::new(val, lock.clone())
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.inner().val.get().cast::<T>()
    }

    pub fn into_inner(self) -> T {
        // Prevent Drop from running
        let ptr = self.ptr as *mut Inner<T>;
        let boxed = unsafe { Box::from_raw(ptr) };

        // SAFETY:
        // - We own the boxed value, so we can read from it safely.
        // - This consumes `self`, so no other thread can access the value anymore.
        let value = unsafe { boxed.val.get().read().assume_init() };

        // Don't drop the box (we just extracted T), let the box go out of scope
        // and drop the AtomicValInner (which no longer contains T)
        mem::forget(boxed); // value was manually extracted, nothing else to drop

        value
    }
}

impl<T: Copy> AtomicCell<T> {
    /// Loads a value from the atomic cell.
    pub fn load(&self) -> T {
        unsafe { (&*self.inner().val.get()).assume_init() }
    }
}

impl<T> AtomicCell<T> {
    /// Atomic store (scrittura atomica con lock esclusivo)
    pub fn store(&self, val: T) {
        if mem::needs_drop::<T>() {
            drop(self.swap(val));
        } else {
            let mut guard = self.get_mut();
            *guard = val;
        }
    }

    /// Atomic swap (scambia il valore e ritorna il precedente)
    pub fn swap(&self, val: T) -> T {
        let mut guard = self.get_mut();
        mem::replace(&mut *guard, val)
    }
}

impl<T: Copy + Eq> AtomicCell<T> {
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

impl<T> Clone for AtomicCell<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicCell<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            unsafe { drop(Box::from_raw(self.ptr as *mut Inner<T>)) };
        }
    }
}
