use crate::mutex::Backoff;
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};

type State = usize;

const UNLOCKED: State = 0;
const READERS_PARKED: State = 0b0001;
const WRITERS_PARKED: State = 0b0010;
const ONE_READER: State = 0b0100;
const ONE_WRITER: State = !(READERS_PARKED | WRITERS_PARKED);

pub struct SpinLockCell<T> {
    state: CachePadded<AtomicUsize>,
    /// Reference counter, updated infrequently
    ref_count: AtomicUsize,
    val: UnsafeCell<T>,
}

#[inline]
fn is_writer_locked(state: State) -> bool {
    (state & ONE_WRITER) == ONE_WRITER
}

/// Extracts the number of active readers from the state.
#[inline]
fn readers_count(state: State) -> usize {
    if is_writer_locked(state) {
        0
    } else {
        (state & ONE_WRITER) >> 2
    }
}

unsafe impl<T: Send> Send for SpinLockCell<T> {}
unsafe impl<T: Send> Sync for SpinLockCell<T> {}

impl<T> SpinLockCell<T> {
    /// Create a new SplLock instance with reference count = 1
    pub fn new(val: T) -> Self {
        Self {
            state: CachePadded::new(AtomicUsize::new(UNLOCKED)),
            ref_count: AtomicUsize::new(1),
            val: UnsafeCell::new(val),
        }
    }

    /// Returns the current reference count.
    pub fn get_ref_count(&self) -> usize {
        self.ref_count.load(Ordering::Acquire)
    }

    /// Returns the number of active shared locks (readers)
    pub fn get_shared_locked(&self) -> usize {
        readers_count(self.state.load(Ordering::Acquire))
    }

    /// Returns true if the mutex is locked exclusively (writer lock)
    #[inline]
    pub fn is_locked_exclusive(&self) -> bool {
        is_writer_locked(self.state.load(Ordering::Relaxed))
    }

    /// Returns true if the mutex is locked in shared mode (reader lock)
    #[inline]
    pub fn is_locked_shared(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        !is_writer_locked(s) && readers_count(s) > 0
    }

    /// Returns true if the mutex is locked in any mode
    pub fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) != UNLOCKED
    }

    /// Acquire exclusive lock with optimized spinning
    pub fn lock_exclusive(&self) -> ExclusiveGuard<'_, T> {
        if self
            .state
            .compare_exchange(UNLOCKED, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return ExclusiveGuard { lock: self };
        }

        self.lock_exclusive_slow()
    }

    /// Release the exclusive lock
    pub fn unlock_exclusive(&self) {
        // Fast path: release without waking other threads
        if self
            .state
            .compare_exchange(ONE_WRITER, UNLOCKED, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        let state = self.state.load(Ordering::Relaxed);

        let parked = state & (READERS_PARKED | WRITERS_PARKED);

        if parked & READERS_PARKED != 0 {
            // Case 1: readers are waiting (possibly also writers)
            self.state.store(
                if parked & WRITERS_PARKED != 0 {
                    WRITERS_PARKED
                } else {
                    UNLOCKED
                },
                Ordering::Release,
            );
        } else if parked & WRITERS_PARKED != 0 {
            // Case 2: only writers are waiting
            self.state.store(UNLOCKED, Ordering::Release);
        }
    }

    fn lock_exclusive_slow(&self) -> ExclusiveGuard<'_, T> {
        let mut acquire_with = UNLOCKED;
        let backoff = Backoff::new();

        let mut state = self.state.load(Ordering::Relaxed);

        loop {
            // Try to acquire the lock if no writer is active
            while state & ONE_WRITER == 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state | ONE_WRITER | acquire_with,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return ExclusiveGuard { lock: self },
                    Err(e) => state = e,
                }
            }

            // Mark writers as parked if not already
            if state & WRITERS_PARKED == 0 {
                if !backoff.is_completed() {
                    backoff.snooze();
                    continue;
                }

                if let Err(e) = self.state.compare_exchange_weak(
                    state,
                    state | WRITERS_PARKED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    state = e;
                    continue;
                }
            }

            backoff.reset();
            while state & ONE_WRITER != 0 {
                backoff.spin();
                state = self.state.load(Ordering::Relaxed);
            }

            backoff.reset();
            acquire_with = WRITERS_PARKED;
        }
    }

    /// Acquire shared lock with optimized spinning
    pub fn lock_shared(&self) -> SharedGuard<'_, T> {
        let state = self.state.load(Ordering::Relaxed);

        if let Some(new_state) = state.checked_add(ONE_READER) {
            if new_state & ONE_WRITER != ONE_WRITER {
                if self
                    .state
                    .compare_exchange(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return SharedGuard { lock: self };
                }
            }
        }

        self.lock_shared_slow()
    }

    fn lock_shared_slow(&self) -> SharedGuard<'_, T> {
        let backoff = Backoff::new();

        loop {
            let mut state = self.state.load(Ordering::Relaxed);

            while let Some(new_state) = state.checked_add(ONE_READER) {
                if self
                    .state
                    .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return SharedGuard { lock: self };
                }

                state = self.state.load(Ordering::Relaxed);
            }

            // Mark readers as parked if not already
            if state & READERS_PARKED == 0 {
                if !backoff.is_completed() {
                    backoff.snooze();
                    continue;
                }

                if self
                    .state
                    .compare_exchange_weak(
                        state,
                        state | READERS_PARKED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue;
                }
            }

            backoff.reset();
            while state & ONE_WRITER == ONE_WRITER {
                backoff.spin();
                state = self.state.load(Ordering::Relaxed);
            }

            backoff.reset();
        }
    }

    #[inline]
    pub fn unlock_shared(&self) {
        let prev_state = self.state.fetch_sub(ONE_READER, Ordering::Release);

        // If last reader and writers are waiting, wake one writer
        if prev_state == (ONE_READER | WRITERS_PARKED) {
            let _ = self.state.compare_exchange(
                WRITERS_PARKED,
                UNLOCKED,
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
    }

    #[inline]
    pub fn unlock_all_shared(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let readers_count = readers_count(state);

            if readers_count == 0 {
                return;
            }

            let mut new_state = state & !ONE_READER.wrapping_mul(readers_count);

            if state & WRITERS_PARKED != 0 {
                new_state |= WRITERS_PARKED;
            }

            if self
                .state
                .compare_exchange(state, new_state, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }
}

/// Exclusive lock guard
pub struct ExclusiveGuard<'a, T> {
    lock: &'a SpinLockCell<T>,
}

impl<'a, T> Deref for ExclusiveGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.val.get() }
    }
}

impl<'a, T> DerefMut for ExclusiveGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<'a, T> Drop for ExclusiveGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock_exclusive();
    }
}

/// Shared lock guard
#[derive(Debug)]
pub struct SharedGuard<'a, T> {
    lock: &'a SpinLockCell<T>,
}

impl<'a, T> Deref for SharedGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.val.get() }
    }
}

impl<'a, T> Drop for SharedGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.unlock_shared();
    }
}

impl<T: fmt::Debug> fmt::Debug for SpinLockCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.load(Ordering::Acquire);
        let readers = state / ONE_READER;
        f.debug_struct("SpinLockCell")
            .field("state", &format!("{:b}", state))
            .field("exclusive_locked", &(state & ONE_WRITER == ONE_WRITER))
            .field("readers_count", &readers)
            .field("readers_parked", &(state & READERS_PARKED != 0))
            .field("writers_parked", &(state & WRITERS_PARKED != 0))
            .finish()
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
            let mut guard = lock.lock_exclusive();
            *guard += 5;
            assert_eq!(*guard, 15);
        }

        {
            let guard2 = lock.lock_exclusive();
            assert_eq!(*guard2, 15);
        }
    }

    #[test]
    fn test_multiple_locks_different_scopes() {
        let lock = SpinLockCell::new(0);

        {
            let mut g1 = lock.lock_exclusive();
            *g1 += 1;
        }

        {
            let mut g2 = lock.lock_exclusive();
            *g2 += 2;
        }

        let g3 = lock.lock_exclusive();
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
                    let mut guard = l.lock_exclusive();
                    *guard += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let g = lock.lock_exclusive();
        assert_eq!(*g, 4000);
    }

    #[test]
    fn test_shared_locking() {
        let lock = Arc::new(SpinLockCell::new(100));
        let mut handles = Vec::new();

        // Multiple readers
        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                let guard = l.lock_shared();
                assert_eq!(*guard, 100);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }
}
