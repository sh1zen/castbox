use crate::mutex::Backoff;
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SpinLockCell<T> {
    state: AtomicUsize,
    val: UnsafeCell<T>,
}

const UPDATING: usize = 0;
const AVAILABLE: usize = 1;

unsafe impl<T: Send> Send for SpinLockCell<T> {}
unsafe impl<T: Send> Sync for SpinLockCell<T> {}

impl<T> SpinLockCell<T> {
    pub fn new(val: T) -> SpinLockCell<T> {
        SpinLockCell {
            val: UnsafeCell::new(val),
            state: AtomicUsize::new(AVAILABLE),
        }
    }

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let backoff = Backoff::new();
        while self
            .state
            .compare_exchange(AVAILABLE, UPDATING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            backoff.snooze();
        }

        SpinLockGuard { lock: self }
    }

    #[inline]
    fn release(&self) {
        self.state.store(AVAILABLE, Ordering::Release);
    }
}

/// Guard che rilascia il lock automaticamente al drop
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLockCell<T>,
}

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.val.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}


#[cfg(test)]
mod tests_splcell {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_single_thread_lock_unlock() {
        let lock = SpinLockCell::new(10);

        {
            let mut guard = lock.lock();
            *guard += 5;
            assert_eq!(*guard, 15);
        } // drop -> unlock automatico

        {
            let guard2 = lock.lock();
            assert_eq!(*guard2, 15);
        }
    }

    #[test]
    fn test_multiple_locks_different_scopes() {
        let lock = SpinLockCell::new(0);

        {
            let mut g1 = lock.lock();
            *g1 += 1;
        } // unlock

        {
            let mut g2 = lock.lock();
            *g2 += 2;
        } // unlock

        let g3 = lock.lock();
        assert_eq!(*g3, 3);
    }

    #[test]
    fn test_multi_thread_increment() {
        let lock = Arc::new(SpinLockCell::new(0usize));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let mut guard = l.lock();
                    *guard += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let g = lock.lock();
        assert_eq!(*g, 4000);
    }

    #[test]
    fn test_lock_guard_drop_releases() {
        let lock = SpinLockCell::new(123);

        {
            let g1 = lock.lock();
            assert_eq!(*g1, 123);
            // non rilascio manualmente, deve farlo Drop
        }

        // Ora posso riacquisire senza problemi
        let g2 = lock.lock();
        assert_eq!(*g2, 123);
    }
}