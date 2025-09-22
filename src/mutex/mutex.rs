use crate::core::futex::{futex_wait, futex_wake, futex_wake_all};
use crate::mutex::Backoff;
use crossbeam_utils::CachePadded;
use std::fmt;
use std::sync::atomic;
use std::sync::atomic::{AtomicUsize, Ordering};

type State = usize;

/// Internal bit flags for the mutex state.
const UNLOCKED: State = 0;
const READERS_PARKED: State = 0b0001;
const WRITERS_PARKED: State = 0b0010;
const ONE_READER: State = 0b0100;
const ONE_WRITER: State = !(READERS_PARKED | WRITERS_PARKED);

/// Internal mutex structure with reference counting and futex-based wait/wake.
struct InnerMutex {
    /// Main mutex state (reader/writer)
    state: CachePadded<AtomicUsize>,
    /// Futex for readers
    readers_futex: CachePadded<AtomicUsize>,
    /// Futex for writers
    writers_futex: CachePadded<AtomicUsize>,
    /// Reference counter, updated infrequently
    ref_count: CachePadded<AtomicUsize>,
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

impl InnerMutex {
    fn new() -> Self {
        Self {
            state: CachePadded::new(AtomicUsize::new(0)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
            readers_futex: CachePadded::new(AtomicUsize::new(0)),
            writers_futex: CachePadded::new(AtomicUsize::new(0)),
        }
    }
}

#[repr(transparent)]
pub struct Mutex {
    ptr: CachePadded<*const InnerMutex>,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

impl std::panic::UnwindSafe for Mutex {}
impl std::panic::RefUnwindSafe for Mutex {}

impl Mutex {
    /// Create a new mutex instance with reference count = 1
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerMutex::new()));
        Self {
            ptr: CachePadded::new(ptr),
        }
    }

    #[inline(always)]
    fn inner(&self) -> &InnerMutex {
        unsafe { &**self.ptr }
    }

    /// Returns the current reference count.
    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Ordering::Acquire)
    }

    /// Returns the number of active shared locks (readers)
    pub fn get_shared_locked(&self) -> usize {
        readers_count(self.inner().state.load(Ordering::Acquire))
    }

    /// Returns true if the mutex is locked exclusively (writer lock)
    #[inline]
    pub fn is_locked_exclusive(&self) -> bool {
        is_writer_locked(self.inner().state.load(Ordering::Acquire))
    }

    /// Returns true if the mutex is locked in shared mode (reader lock)
    #[inline]
    pub fn is_locked_shared(&self) -> bool {
        let s = self.inner().state.load(Ordering::Acquire);
        !is_writer_locked(s) && readers_count(s) > 0
    }

    /// Returns true if the mutex is locked in any mode
    pub fn is_locked(&self) -> bool {
        self.inner().state.load(Ordering::Acquire) != UNLOCKED
    }

    /// Try to acquire the exclusive lock without blocking
    #[inline]
    pub fn try_lock_exclusive(&self) -> bool {
        self.inner()
            .state
            .compare_exchange(UNLOCKED, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Acquire the exclusive lock (writer). Blocks if unavailable
    pub fn lock_exclusive(&self) {
        if self
            .inner()
            .state
            .compare_exchange(UNLOCKED, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_exclusive_slow();
        }
    }

    /// Release the exclusive lock
    pub fn unlock_exclusive(&self) {
        // Fast path: release without waking other threads
        if self
            .inner()
            .state
            .compare_exchange(ONE_WRITER, UNLOCKED, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            // Slow path: there are parked threads
            self.unlock_exclusive_slow();
        }
    }

    /// Slow path for acquiring the exclusive lock with contention handling
    #[cold]
    fn lock_exclusive_slow(&self) {
        let inner = self.inner();
        let mut acquire_with = UNLOCKED;
        let backoff = Backoff::new();

        let mut state = inner.state.load(Ordering::Relaxed);

        loop {
            // Try to acquire the lock if no writer is active
            while state & ONE_WRITER == 0 {
                match inner.state.compare_exchange_weak(
                    state,
                    state | ONE_WRITER | acquire_with,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(e) => state = e,
                }
            }

            // Mark writers as parked if not already
            if state & WRITERS_PARKED == 0 {
                if !backoff.is_completed() {
                    backoff.snooze();
                    continue;
                }

                if let Err(e) = inner.state.compare_exchange_weak(
                    state,
                    state | WRITERS_PARKED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    state = e;
                    continue;
                }
            }

            // Park thread until lock becomes available
            loop {
                let state = inner.state.load(Ordering::Acquire);
                if (state & ONE_WRITER == 0) || (state & WRITERS_PARKED == 0) {
                    break;
                }

                // Use futex sequence number to avoid lost wakeups
                let w_key = inner.writers_futex.load(Ordering::Acquire);
                futex_wait(&inner.writers_futex, w_key);
            }

            backoff.reset();
            acquire_with = WRITERS_PARKED;
            state = inner.state.load(Ordering::Relaxed);
        }
    }

    /// Slow path for releasing exclusive lock, wakes waiting threads if needed
    #[inline]
    fn unlock_exclusive_slow(&self) {
        let inner = self.inner();
        let state = inner.state.load(Ordering::Relaxed);

        let parked = state & (READERS_PARKED | WRITERS_PARKED);

        if parked & READERS_PARKED != 0 {
            // Case 1: readers are waiting (possibly also writers)
            inner.state.store(
                if parked & WRITERS_PARKED != 0 {
                    WRITERS_PARKED
                } else {
                    UNLOCKED
                },
                Ordering::Release,
            );
            inner.readers_futex.fetch_add(1, Ordering::Release);
            futex_wake_all(&*inner.readers_futex);
        } else if parked & WRITERS_PARKED != 0 {
            // Case 2: only writers are waiting
            inner.state.store(UNLOCKED, Ordering::Release);
            inner.writers_futex.fetch_add(1, Ordering::Release);
            futex_wake(&*inner.writers_futex);
        }
    }

    /// Try to acquire a shared lock (reader). Returns false if unavailable
    #[inline]
    fn try_lock_shared(&self) -> bool {
        self.try_lock_shared_fast() || self.try_lock_shared_slow()
    }

    /// Fast path for acquiring a shared lock
    #[inline(always)]
    fn try_lock_shared_fast(&self) -> bool {
        let inner = self.inner();
        let state = inner.state.load(Ordering::Relaxed);

        if let Some(new_state) = state.checked_add(ONE_READER) {
            if new_state & ONE_WRITER != ONE_WRITER {
                return inner
                    .state
                    .compare_exchange(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok();
            }
        }

        false
    }

    /// Slow path for acquiring a shared lock under contention
    #[cold]
    fn try_lock_shared_slow(&self) -> bool {
        let inner = self.inner();
        let mut state = inner.state.load(Ordering::Relaxed);

        while let Some(new_state) = state.checked_add(ONE_READER) {
            if new_state & ONE_WRITER == ONE_WRITER {
                break;
            }

            match inner.state.compare_exchange_weak(
                state,
                new_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(e) => state = e,
            }
        }

        false
    }

    /// Acquire a shared lock (reader). Blocks if unavailable
    pub fn lock_shared(&self) {
        if !self.try_lock_shared_fast() {
            self.lock_shared_slow();
        }
    }

    /// Release a shared lock (reader)
    pub fn unlock_shared(&self) {
        let inner = self.inner();
        let prev_state = inner.state.fetch_sub(ONE_READER, Ordering::Release);

        // If last reader and writers are waiting, wake one writer
        if prev_state == (ONE_READER | WRITERS_PARKED) {
            if inner
                .state
                .compare_exchange(
                    WRITERS_PARKED,
                    UNLOCKED,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                inner.writers_futex.fetch_add(1, Ordering::Release);
                futex_wake(&*inner.writers_futex);
            }
        }
    }

    /// Slow path for acquiring a shared lock, parks while writers are active
    #[cold]
    fn lock_shared_slow(&self) {
        let inner = self.inner();
        let backoff = Backoff::new();

        loop {
            let mut state = inner.state.load(Ordering::Relaxed);

            while let Some(new_state) = state.checked_add(ONE_READER) {
                if inner
                    .state
                    .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }

                state = inner.state.load(Ordering::Relaxed);
            }

            // Mark readers as parked if not already
            if state & READERS_PARKED == 0 {
                if !backoff.is_completed() {
                    backoff.snooze();
                    continue;
                }

                if inner
                    .state
                    .compare_exchange_weak(
                        state,
                        state | READERS_PARKED,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue;
                }
            }

            // Park until a writer releases the lock
            loop {
                let state = inner.state.load(Ordering::Relaxed);
                if (state & ONE_WRITER != ONE_WRITER) || (state & READERS_PARKED == 0) {
                    break;
                }
                let w_key = inner.readers_futex.load(Ordering::Acquire);
                futex_wait(&inner.readers_futex, w_key);
            }

            backoff.reset();
        }
    }

    /// Release all shared locks held by this thread at once
    /// Useful for bulk operations that release many readers simultaneously
    pub fn unlock_all_shared(&self) {
        let inner = self.inner();

        loop {
            let state = inner.state.load(Ordering::Relaxed);
            let readers_count = readers_count(state);

            if readers_count == 0 {
                return;
            }

            let mut new_state = state & !(ONE_READER.wrapping_mul(readers_count));

            if state & WRITERS_PARKED != 0 {
                new_state |= WRITERS_PARKED;
            }

            if inner
                .state
                .compare_exchange(state, new_state, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                if state & WRITERS_PARKED != 0 {
                    inner.writers_futex.fetch_add(1, Ordering::Release);
                    futex_wake(&*inner.writers_futex);
                }
                break;
            }
        }
    }

    /// Downgrade an exclusive (writer) lock into a shared (reader) lock
    /// Allows the current writer to continue holding a read lock
    pub fn downgrade(&self) {
        let inner = self.inner();

        let mut state = inner.state.load(Ordering::Relaxed);
        loop {
            let new_state = (state & !ONE_WRITER) + ONE_READER;
            match inner.state.compare_exchange(
                state,
                new_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if state & READERS_PARKED != 0 {
                        futex_wake_all(&*inner.readers_futex);
                    }
                    break;
                }
                Err(s) => state = s,
            }
        }
    }
}

impl Clone for Mutex {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Mutex { ptr: self.ptr }
    }
}

impl Drop for Mutex {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            let ptr = *self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner().state.load(Ordering::Acquire);
        let readers = readers_count(state);

        f.debug_struct("Mutex")
            .field("state", &format!("{:b}", state))
            .field("exclusive_locked", &self.is_locked_exclusive())
            .field("readers_count", &readers)
            .field("readers_parked", &(state & READERS_PARKED != 0))
            .field("writers_parked", &(state & WRITERS_PARKED != 0))
            .field("ref_count", &self.inner().ref_count.load(Ordering::Acquire))
            .finish()
    }
}
