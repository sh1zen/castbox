use crate::core::futex::{futex_wait, futex_wake, futex_wake_all};
use crate::core::smutex::SGuard;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct SCondVar {
    waiters: AtomicUsize,
}

impl SCondVar {
    #[inline]
    pub(crate) const fn new() -> Self {
        Self {
            waiters: AtomicUsize::new(1),
        }
    }

    pub(crate) fn wait<'a>(&self, guard: SGuard<'a>) -> SGuard<'a> {
        loop {
            let futex_value = self.waiters.load(Ordering::Acquire);

            // Unlock the mutex before going to sleep.
            SGuard::unlock(&guard);

            // Wait, but only if there hasn't been any
            // notification since we unlocked the mutex.
            futex_wait(&self.waiters, futex_value);

            // Lock the mutex again.
            SGuard::lock(&guard);

            // se epoch è cambiato → notifica ricevuta
            if self.waiters.load(Ordering::Acquire) != futex_value {
                break;
            }
        }

        guard
    }

    // All the memory orderings here are `Relaxed`,
    // because synchronization is done by unlocking and locking the mutex.
    pub(crate) fn notify_one(&self) {
        self.waiters.fetch_add(1, Relaxed);
        futex_wake(&self.waiters);
    }

    pub(crate) fn notify_all(&self) {
        self.waiters.fetch_add(1, Relaxed);
        futex_wake_all(&self.waiters);
    }
}

#[cfg(test)]
mod tests_scondvar {
    use super::*;
    use crate::core::smutex::SMutex;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_notify_one() {
        let mutex = Arc::new(SMutex::new());
        let condvar = Arc::new(SCondVar::new());

        let m = mutex.clone();
        let cv = condvar.clone();

        let handle = thread::spawn(move || {
            let m = m.lock();
            cv.wait(m);
            42
        });

        thread::sleep(Duration::from_millis(50));

        condvar.notify_one();

        let result = handle.join().unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_notify_all() {
        let mutex = Arc::new(SMutex::new());
        let condvar = Arc::new(SCondVar::new());

        let mut handles = vec![];
        for _ in 0..3 {
            let m = mutex.clone();
            let cv = condvar.clone();
            handles.push(thread::spawn(move || {
                let m = m.lock();
                cv.wait(m);
                1
            }));
        }

        thread::sleep(Duration::from_millis(50));

        condvar.notify_all();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results, vec![1, 1, 1]);
    }
}
