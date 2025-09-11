use crate::core::futex::{futex_wait, futex_wake_all};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

const UNLOCKED: usize = 0;
const ONE_READER: usize = 0b0010;
const ONE_WRITER: usize = 0b0001;

#[repr(transparent)]
pub struct RwFutex {
    state: AtomicUsize,
}

unsafe impl Send for RwFutex {}
unsafe impl Sync for RwFutex {}

impl RwFutex {
    pub fn new() -> Self {
        Self {
            state: AtomicUsize::new(UNLOCKED),
        }
    }

    // ======================
    // WRITER
    // ======================
    pub fn lock_exclusive(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state == UNLOCKED {
                if self
                    .state
                    .compare_exchange(UNLOCKED, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
            }
            futex_wait(&self.state, state);
        }
    }

    pub fn unlock_exclusive(&self) {
        self.state.store(UNLOCKED, Ordering::Release);
        futex_wake_all(&self.state);
    }

    // ======================
    // READER
    // ======================
    pub fn lock_shared(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state & ONE_WRITER == 0 {
                let readers = state & !ONE_WRITER;
                let new_state = readers + ONE_READER;
                if self
                    .state
                    .compare_exchange(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
            }
            futex_wait(&self.state, state);
        }
    }

    pub fn unlock_shared(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let readers = state & !ONE_WRITER;
            let new_state = (state & ONE_WRITER) | (readers - ONE_READER);
            if self
                .state
                .compare_exchange(state, new_state, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                if new_state == 0 {
                    futex_wake_all(&self.state);
                }
                return;
            }
        }
    }

    pub fn readers(&self) -> usize {
        self.state.load(Ordering::Acquire) & !ONE_WRITER
    }

    pub fn is_writer_locked(&self) -> bool {
        self.state.load(Ordering::Acquire) & ONE_WRITER != 0
    }
}

impl fmt::Debug for RwFutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwFutex")
            .field("writer_locked", &self.is_writer_locked())
            .field("readers", &self.readers())
            .finish()
    }
}
