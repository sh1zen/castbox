use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use std::cell::UnsafeCell;

pub struct MutexG<T: ?Sized> {
    lock: Mutex,
    data: UnsafeCell<T>,
}

impl<T> MutexG<T> {
    pub fn new(data: T) -> Self {
        Self {
            lock: Mutex::new(),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T: ?Sized> MutexG<T> {
    pub fn lock_shared(&self) -> WatchGuardRef<'_, T> {
        // SAFETY: We ensure shared access via lock
        let data = unsafe { &*self.data.get() };
        WatchGuardRef::new(data, self.lock.clone())
    }

    pub fn lock_exclusive(&self) -> WatchGuardMut<'_, T> {
        // SAFETY: We ensure exclusive access via lock
        let data = unsafe { &mut *self.data.get() };
        WatchGuardMut::new(data, self.lock.clone())
    }
}
