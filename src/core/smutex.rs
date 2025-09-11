use crate::core::futex::{futex_wait, futex_wake};
use crate::mutex::Backoff;
use crossbeam_utils::CachePadded;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Internal bit flags for the mutex state.
const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const GROUP_FLAG: usize = 2; // Indicates that group (shared) locks are active

/// A lightweight futex-based spin mutex supporting both
/// exclusive and group (shared) locking modes.
///
/// - Fast path: lock acquisition via atomic CAS.
/// - Slow path: adaptive spinning with futex sleep.
/// - Group locks can coexist with others, similar to read locks.
pub(crate) struct SMutex {
    state: CachePadded<AtomicUsize>,
}

impl SMutex {
    /// Creates a new unlocked mutex.
    pub(crate) fn new() -> Self {
        Self {
            state: CachePadded::new(AtomicUsize::new(UNLOCKED)),
        }
    }

    /// Returns `true` if the mutex is currently locked (either exclusive or group).
    pub(crate) fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) & LOCKED != 0
    }

    /// Acquires the mutex in exclusive mode (blocking).
    pub(crate) fn lock(&self) -> SGuard<'_> {
        self.raw_lock();
        SGuard::new(self)
    }

    /// Acquires the mutex in group (shared) mode (blocking).
    pub(crate) fn lock_group(&self) -> SGuard<'_> {
        self.raw_lock_group();
        SGuard::new_group(self)
    }

    /// Attempts to lock the mutex in exclusive mode (fast path).
    /// Falls back to the slow path on contention.
    pub(crate) fn raw_lock(&self) {
        if self
            .state
            .compare_exchange_weak(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.raw_lock_slow();
        }
    }

    /// Locks the mutex in group (shared) mode.
    /// Allows multiple concurrent holders unless a writer is active.
    fn raw_lock_group(&self) {
        let spin = Backoff::new();
        loop {
            let mut state = self.state.load(Ordering::Relaxed);

            // If no writer is active, add the group flag
            while state & LOCKED == 0 {
                let new_state = state | GROUP_FLAG;
                match self.state.compare_exchange_weak(
                    state,
                    new_state,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(e) => state = e,
                }
            }

            // If a writer holds the lock, spin or sleep
            if state & LOCKED != 0 {
                if !spin.is_yielding() {
                    spin.snooze();
                    continue;
                }

                // Wait until writer releases the lock
                while self.state.load(Ordering::Relaxed) & LOCKED != 0 {
                    futex_wait(&self.state, state);
                }
            }
        }
    }

    /// Slow path for exclusive locking.
    /// Spins briefly, then waits on futex until the mutex becomes available.
    fn raw_lock_slow(&self) {
        let spin = Backoff::new();
        loop {
            let mut state = self.state.load(Ordering::Relaxed);

            // Try to acquire the lock if currently unlocked
            while state == UNLOCKED {
                match self.state.compare_exchange_weak(
                    UNLOCKED,
                    LOCKED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(e) => state = e,
                }
            }

            // If already locked, spin and eventually sleep
            if state == LOCKED {
                if !spin.is_yielding() {
                    spin.snooze();
                    continue;
                }

                while self.state.load(Ordering::Relaxed) == LOCKED {
                    futex_wait(&self.state, LOCKED);
                }
            }
        }
    }

    /// Unlocks an exclusive lock and wakes up one waiter.
    pub(crate) fn raw_unlock(&self) {
        self.state.fetch_and(!LOCKED, Ordering::Release);
        futex_wake(&*self.state);
    }

    /// Unlocks a group (shared) lock and wakes up one waiter.
    pub(crate) fn raw_unlock_group(&self) {
        self.state.fetch_and(!GROUP_FLAG, Ordering::Release);
        futex_wake(&*self.state);
    }
}

/// RAII guard for SMutex that automatically unlocks on drop.
pub(crate) struct SGuard<'a> {
    m: &'a SMutex,
    is_group: bool,
}

impl<'a> SGuard<'a> {
    /// Creates a new exclusive guard.
    fn new(m: &'a SMutex) -> SGuard<'a> {
        SGuard { m, is_group: false }
    }

    /// Creates a new group (shared) guard.
    pub(crate) fn new_group(m: &'a SMutex) -> SGuard<'a> {
        SGuard { m, is_group: true }
    }

    /// Explicit unlock (optional manual release before drop).
    pub(crate) fn unlock(this: &SGuard<'_>) {
        if this.is_group {
            this.m.raw_unlock_group();
        } else {
            this.m.raw_unlock();
        }
    }

    /// Explicit re-lock (optional reacquisition).
    pub(crate) fn lock(this: &SGuard<'_>) {
        if this.is_group {
            this.m.raw_lock_group();
        } else {
            this.m.raw_lock();
        }
    }
}

impl<'a> Drop for SGuard<'a> {
    fn drop(&mut self) {
        if self.is_group {
            self.m.raw_unlock_group();
        } else {
            self.m.raw_unlock();
        }
    }
}
