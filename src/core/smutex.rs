use crate::core::futex::{futex_wait, futex_wake};
use crate::mutex::Backoff;
use std::sync::atomic::{AtomicUsize, Ordering};

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const GROUP_FLAG: usize = 2; // indica che ci sono lock di gruppo attivi

pub(crate) struct SMutex {
    state: AtomicUsize,
}

impl SMutex {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(UNLOCKED),
        }
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) & LOCKED != 0
    }

    pub(crate) fn lock(&self) -> SGuard<'_> {
        self.raw_lock();
        SGuard::new(self)
    }

    pub(crate) fn lock_group(&self) -> SGuard<'_> {
        self.raw_lock_group();
        SGuard::new(self)
    }

    pub(crate) fn raw_lock(&self) {
        if self
            .state
            .compare_exchange_weak(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.raw_lock_slow();
        }
    }

    fn raw_lock_group(&self) {
        let spin = Backoff::new();
        loop {
            let mut state = self.state.load(Ordering::Relaxed);

            // Se non ci sono writer, possiamo aggiungere un lettore di gruppo
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

            // Se c'è un writer, attendiamo
            if state & LOCKED != 0 {
                if !spin.is_yielding() {
                    spin.snooze();
                    continue;
                }

                while self.state.load(Ordering::Relaxed) & LOCKED != 0 {
                    futex_wait(&self.state, state);
                }
            }
        }
    }

    fn raw_lock_slow(&self) {
        let spin = Backoff::new();
        loop {
            let mut state = self.state.load(Ordering::Relaxed);

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

    pub(crate) fn raw_unlock(&self) {
        self.state.fetch_and(!LOCKED, Ordering::Release);
        futex_wake(&self.state);
    }

    pub(crate) fn raw_unlock_group(&self) {
        self.state.fetch_and(!GROUP_FLAG, Ordering::Release);
        futex_wake(&self.state);
    }
}

pub(crate) struct SGuard<'a> {
    m: &'a SMutex,
    is_group: bool,
}

impl<'a> SGuard<'a> {
    fn new(m: &'a SMutex) -> SGuard<'a> {
        SGuard { m, is_group: false }
    }

    pub(crate) fn new_group(m: &'a SMutex) -> SGuard<'a> {
        SGuard { m, is_group: true }
    }

    // Unlock esplicito
    pub(crate) fn unlock(this: &SGuard<'_>) {
        if this.is_group {
            this.m.raw_unlock_group();
        } else {
            this.m.raw_unlock();
        }
    }

    // Lock esplicito (riacquisizione)
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
