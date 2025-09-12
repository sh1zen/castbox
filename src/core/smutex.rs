use crate::core::futex::{futex_wait, futex_wake};
use std::sync::atomic::{AtomicUsize, Ordering};

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;

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
        self.state.load(Ordering::Relaxed) == LOCKED
    }

    pub(crate) fn lock(&self) -> SGuard<'_> {
        self.raw_lock();
        SGuard::new(self)
    }

    pub(crate) fn raw_lock(&self) {
        if self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }

        // Altrimenti entriamo in attesa
        loop {
            // Aspettiamo finché lo stato rimane LOCKED
            while self.state.load(Ordering::Relaxed) == LOCKED {
                futex_wait(&self.state, LOCKED);
            }

            // Ritentiamo l'acquisizione
            if self
                .state
                .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(crate) fn raw_unlock(&self) {
        self.state.store(UNLOCKED, Ordering::Release);
        futex_wake(&self.state);
    }
}

pub(crate) struct SGuard<'a> {
    m: &'a SMutex,
}

impl<'a> SGuard<'a> {
    fn new(m: &'a SMutex) -> SGuard<'a> {
        SGuard { m }
    }

    pub(crate) fn lock(this: &SGuard<'_>) {
        this.m.raw_lock();
    }

    pub(crate) fn unlock(this: &SGuard<'_>) {
        this.m.raw_unlock();
    }
}

impl<'a> Drop for SGuard<'a> {
    fn drop(&mut self) {
        self.m.raw_unlock()
    }
}
