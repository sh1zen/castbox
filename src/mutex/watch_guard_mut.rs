use crate::mutex::Mutex;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use crossbeam_utils::CachePadded;

/// used as wrapper for a pointer to a reference
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct WatchGuardMut<'a, T: ?Sized> {
    data: *mut T,
    lock: CachePadded<Mutex>,
    marker: PhantomData<&'a mut T>,
}

impl<'mutex, T: ?Sized> WatchGuardMut<'mutex, T> {
    ///create a new WatchGuard from a &mut T and AnyRef
    pub fn new(ptr: *mut T, lock: Mutex) -> WatchGuardMut<'mutex, T> {
        Self {
            data: ptr,
            lock: CachePadded::new(lock),
            marker: PhantomData,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked_exclusive()
    }
}

unsafe impl<T: ?Sized + Sync> Sync for WatchGuardMut<'_, T> {}
unsafe impl<T: ?Sized + Send> Send for WatchGuardMut<'_, T> {}

impl<T: ?Sized> Deref for WatchGuardMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        debug_assert!(self.lock.is_locked_exclusive(), "{:?}", self.lock);
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for WatchGuardMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        debug_assert!(self.lock.is_locked_exclusive(), "{:?}", self.lock);
        unsafe { &mut *self.data }
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
        unsafe { &*self.data == other }
    }
}

impl<'a, T: Debug> Debug for WatchGuardMut<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuardRef")
            .field("data", &self.data)
            .field("lock", &self.lock)
            .finish()
    }
}
