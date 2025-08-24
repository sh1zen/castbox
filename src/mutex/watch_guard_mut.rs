use crate::mutex::Mutex;
use std::fmt::{Debug, Formatter};
use std::ops::{Deref, DerefMut};

/// used as wrapper for a pointer to a reference
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct WatchGuardMut<'a, T: ?Sized> {
    data: &'a mut T,
    lock: Mutex,
}

impl<'mutex, T: ?Sized> WatchGuardMut<'mutex, T> {
    ///create a new WatchGuard from a &mut T and AnyRef
    pub fn new(ptr: &'mutex mut T, lock: Mutex) -> WatchGuardMut<'mutex, T> {
        Self { data: ptr, lock }
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked_exclusive()
    }
}

/// `T` must be `Sync` for a [`WatchGuardMut<T>`] to be `Sync`
/// because it is possible to get a `&T` from `&WatchGuard` (via `Deref`).
unsafe impl<T: ?Sized + Sync> Sync for WatchGuardMut<'_, T> {}

impl<T: ?Sized> Deref for WatchGuardMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &*self.data
    }
}

impl<T: ?Sized> DerefMut for WatchGuardMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut *self.data
    }
}

impl<T: ?Sized> Drop for WatchGuardMut<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock_exclusive();
    }
}

impl<'a, T, U> PartialEq<U> for WatchGuardMut<'a, T>
where
    T: PartialEq<U> + ?Sized,
{
    fn eq(&self, other: &U) -> bool {
        self.data == other
    }
}

impl<'a, T: Debug> Debug for WatchGuardMut<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuardRef")
            .field("data", self.data)
            .field("lock", &self.lock)
            .finish()
    }
}
