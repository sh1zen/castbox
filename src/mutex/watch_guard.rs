use crate::mutex::Mutex;
use std::fmt::{Debug, Formatter};
use std::ops::{Deref, DerefMut};

/// used as wrapper for a pointer to a reference
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct WatchGuard<T: Sized> {
    data: T,
    lock: Mutex,
}

impl<T> WatchGuard<T> {
    ///create a new WatchGuard from a &mut T and AnyRef
    pub fn new(data: T) -> WatchGuard<T> {
        let lock = Mutex::new();
        Self { data, lock }
    }

    pub fn new_locked(data: T, lock: Mutex) -> WatchGuard<T> {
        Self { data, lock }
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked_exclusive()
    }

    pub fn lock(&self) {
        self.lock.lock_exclusive();
    }

    pub fn unlock(&self) {
        self.lock.unlock_exclusive();
    }
}

unsafe impl<T: Sync> Sync for WatchGuard<T> {}
unsafe impl<T: Send> Send for WatchGuard<T> {}

impl<T> Deref for WatchGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.data
    }
}

impl<T> DerefMut for WatchGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<T> Drop for WatchGuard<T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock_group();
    }
}

impl<T, U> PartialEq<U> for WatchGuard<T>
where
    T: PartialEq<U>,
{
    fn eq(&self, other: &U) -> bool {
        self.data == *other
    }
}

impl<T: Debug> Debug for WatchGuard<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuard")
            .field("data", &self.data)
            .field("lock", &self.lock)
            .finish()
    }
}
