use crate::mutex::Mutex;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;

/// used as wrapper for a pointer to a reference
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct WatchGuardRef<'a, T: ?Sized> {
    data: &'a T,
    lock: Mutex,
}

impl<'mutex, T: ?Sized> WatchGuardRef<'mutex, T> {
    ///create a new WatchGuard from a &mut T and AnyRef
    pub fn new(ptr: &'mutex T, lock: Mutex) -> WatchGuardRef<'mutex, T> {
        Self { data: ptr, lock }
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked_exclusive()
    }
}

/// `T` must be `Sync` for a [`WatchGuard<T>`] to be `Sync`
/// because it is possible to get a `&T` from `&WatchGuard` (via `Deref`).
unsafe impl<T: ?Sized + Sync> Sync for WatchGuardRef<'_, T> {}

impl<T: ?Sized> Deref for WatchGuardRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &*self.data
    }
}

impl<T: ?Sized> Drop for WatchGuardRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock_group();
    }
}

impl<'a, T, U> PartialEq<U> for WatchGuardRef<'a, T>
where
    T: PartialEq<U> + ?Sized,
{
    fn eq(&self, other: &U) -> bool {
        self.data == other
    }
}

impl<'a, T: Debug> Debug for WatchGuardRef<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuardRef")
            .field("data", self.data)
            .field("lock", &self.lock)
            .finish()
    }
}
