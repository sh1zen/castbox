use crate::mutex::Mutex;
use std::ops::{Deref, DerefMut};

/// used as wrapper for a pointer to a reference
#[must_use = "if unused the Mutex will immediately unlock"]
#[clippy::has_significant_drop]
#[derive(Debug)]
pub struct WatchGuard<'a, T: ?Sized> {
    data: &'a mut T,
    lock: Mutex,
}

impl<'mutex, T: ?Sized> WatchGuard<'mutex, T> {
    ///create a new WatchGuard from a &mut T and AnyRef
    pub fn new(ptr: &'mutex mut T, lock: Mutex) -> WatchGuard<'mutex, T> {
        Self { data: ptr, lock }
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }
}

/// `T` must be `Sync` for a [`WatchGuard<T>`] to be `Sync`
/// because it is possible to get a `&T` from `&WatchGuard` (via `Deref`).
unsafe impl<T: ?Sized + Sync> Sync for WatchGuard<'_, T> {}

impl<T: ?Sized> Deref for WatchGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &*self.data
    }
}

impl<T: ?Sized> DerefMut for WatchGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut *self.data
    }
}

impl<T: ?Sized> Drop for WatchGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock();
    }
}
