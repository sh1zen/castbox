use crate::core::smutex::SMutex;
use crate::core::thread::{ThreadParker, Unparker};
use crate::collections::AtomicVec;
use std::fmt;
use std::sync::atomic::{fence, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

pub(crate) enum MutexType {
    Exclusive,
    Group,
}

type State = usize;

const READERS_PARKED: State = 0b0001;
const WRITERS_PARKED: State = 0b0010;
const ONE_READER: State = 0b0100;
const ONE_WRITER: State = !(READERS_PARKED | WRITERS_PARKED);

#[inline]
fn is_writer_locked(state: State) -> bool {
    (state & ONE_WRITER) == ONE_WRITER
}

#[inline]
fn reader_count(state: State) -> usize {
    if is_writer_locked(state) {
        0
    } else {
        (state & ONE_WRITER) >> 2
    }
}

struct InnerMutex {
    /// Atomic state: reader count in multiples of ONE_READER; flags in low bits; writer-locked is ONE_WRITER (+ optional flags)
    state: AtomicUsize,
    /// Small mutex protecting wait queues only (not the main state)
    wait_mutex: SMutex,
    /// Queue of waiters for group (shared) lock
    readers_waiters: AtomicVec<Unparker>,
    /// Queue of waiters for exclusive lock
    writers_waiters: AtomicVec<Unparker>,
    /// Reference count for outer Mutex clones
    ref_count: AtomicUsize,
}

unsafe impl Send for InnerMutex {}
unsafe impl Sync for InnerMutex {}

impl InnerMutex {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            wait_mutex: SMutex::new(),
            readers_waiters: AtomicVec::new(),
            writers_waiters: AtomicVec::new(),
            ref_count: AtomicUsize::new(1),
        }
    }

}

#[repr(transparent)]
pub struct Mutex {
    ptr: *const InnerMutex,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

impl std::panic::UnwindSafe for Mutex {}
impl std::panic::RefUnwindSafe for Mutex {}

impl Mutex {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerMutex::new()));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mutex");
        }
        Self { ptr }
    }

    #[inline]
    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Ordering::Acquire)
    }

    pub fn get_group_locked(&self) -> usize {
        let s = self.inner().state.load(Ordering::Acquire);
        reader_count(s)
    }

    pub fn is_locked_group(&self) -> bool {
        let s = self.inner().state.load(Ordering::Relaxed);
        !is_writer_locked(s) && reader_count(s) > 0
    }

    pub fn is_locked_exclusive(&self) -> bool {
        let s = self.inner().state.load(Ordering::Relaxed);
        is_writer_locked(s)
    }

    pub fn is_locked(&self) -> bool {
        let s = self.inner().state.load(Ordering::Relaxed);
        is_writer_locked(s) || reader_count(s) > 0
    }

    #[inline]
    fn spin(&self, _spin: usize) -> State {
        self.inner().state.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn lock_exclusive(&self) {
        let st = &self.inner().state;
        if st
            .compare_exchange(0, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.lock_exclusive_slow();
    }

    #[cold]
    fn lock_exclusive_slow(&self) {
        let inner = self.inner();
        loop {
            // Try to acquire if no readers and no writer (only flags allowed)
            let mut state = inner.state.load(Ordering::Relaxed);
            // Quick path: if upper bits are zero (no readers/writer)
            while (state & ONE_WRITER) == 0 {
                let desired = ONE_WRITER | (state & (READERS_PARKED | WRITERS_PARKED));
                match inner.state.compare_exchange_weak(
                    state,
                    desired,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(s) => state = s,
                }
            }

            // Bounded spin to avoid parking if the lock becomes free quickly
            let mut spins = 32u32;
            while spins > 0 {
                let s = inner.state.load(Ordering::Relaxed);
                if (s & ONE_WRITER) == 0 {
                    let desired = ONE_WRITER | (s & (READERS_PARKED | WRITERS_PARKED));
                    if inner
                        .state
                        .compare_exchange_weak(
                            s,
                            desired,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                core::hint::spin_loop();
                spins -= 1;
            }

            // Ensure WRITERS_PARKED is set (no need to loop; just set the bit)
            inner.state.fetch_or(WRITERS_PARKED, Ordering::Relaxed);

            // Park current thread in writers queue
            let (parker, unparker) = ThreadParker::new();
            {
                let _g = inner.wait_mutex.lock();
                inner.writers_waiters.push(unparker);
            }
            parker.park();
            // try again
        }
    }

    #[inline(always)]
    pub fn lock_group(&self) {
        if self.try_lock_shared_fast() {
            return;
        }
        self.lock_shared_slow();
    }

    #[inline(always)]
    fn try_lock_shared_fast(&self) -> bool {
        let inner = self.inner();
        let state = inner.state.load(Ordering::Relaxed);
        if let Some(new_state) = state.checked_add(ONE_READER) {
            if (new_state & ONE_WRITER) != ONE_WRITER {
                return inner
                    .state
                    .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok();
            }
        }
        false
    }

    #[cold]
    fn lock_shared_slow(&self) {
        let inner = self.inner();
        loop {
            // Try to acquire shared quickly
            let mut state = inner.state.load(Ordering::Relaxed);
            // Attempt to add a reader while no writer locked
            while let Some(new_state) = state.checked_add(ONE_READER) {
                if (new_state & ONE_WRITER) == ONE_WRITER {
                    break;
                }
                if inner
                    .state
                    .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                state = inner.state.load(Ordering::Relaxed);
            }

            // Set READERS_PARKED flag
            let _ = inner.state.fetch_or(READERS_PARKED, Ordering::Relaxed);

            // Park current thread in readers queue
            let (parker, unparker) = ThreadParker::new();
            {
                let _g = inner.wait_mutex.lock();
                inner.readers_waiters.push(unparker);
            }
            parker.park();
            // retry loop
        }
    }

    #[inline(always)]
    pub fn unlock_group(&self) {
        let inner = self.inner();
        let prev = inner.state.fetch_sub(ONE_READER, Ordering::Release);
        // If this was the last reader and writers are parked, wake one writer.
        if prev == (ONE_READER | WRITERS_PARKED) {
            // Attempt to clear WRITERS_PARKED -> 0
            if inner
                .state
                .compare_exchange(WRITERS_PARKED, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // Wake a single writer
                let to_wake;
                {
                    let _g = inner.wait_mutex.lock();
                    to_wake = inner.writers_waiters.pop();
                }
                if let Some(u) = to_wake {
                    u.unpark();
                }
            }
        }
    }

    #[inline(always)]
    pub fn unlock_exclusive(&self) {
        let inner = self.inner();
        // Fast path: no one is waiting
        if inner
            .state
            .compare_exchange(ONE_WRITER, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.unlock_exclusive_slow();
    }

    #[cold]
    fn unlock_exclusive_slow(&self) {
        let inner = self.inner();
        let state = inner.state.load(Ordering::Relaxed);
        debug_assert!(
            is_writer_locked(state),
            "Is not Locked Exclusive (state = {state})"
        );

        let parked = state & (READERS_PARKED | WRITERS_PARKED);

        if parked != (READERS_PARKED | WRITERS_PARKED) {
            if let Err(new_state) =
                inner
                    .state
                    .compare_exchange(state, 0, Ordering::Release, Ordering::Relaxed)
            {
                // another waiter might have set both flags; treat as both parked
                let _ = new_state; // silence warning
            } else {
                // we cleared state to 0; wake according to parked
                if parked == READERS_PARKED {
                    self.unpark_all_readers();
                    return;
                } else {
                    self.unpark_one_writer();
                    return;
                }
            }
        }

        // both parked: let readers proceed first, keep WRITERS_PARKED set
        inner.state.store(WRITERS_PARKED, Ordering::Release);
        self.unpark_all_readers();
    }

    pub fn unlock_all_group(&self) {
        let inner = self.inner();
        // Clear all readers (forcefully), keep only parked flags
        loop {
            let s = inner.state.load(Ordering::Relaxed);
            if is_writer_locked(s) {
                // if writer is locked, nothing to clear for readers
                break;
            }
            let flags = s & (READERS_PARKED | WRITERS_PARKED);
            if (s & ONE_WRITER) == flags {
                break; // already no readers
            }
            if inner
                .state
                .compare_exchange(s, flags, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        // Wake one writer if parked and all readers cleared
        self.unpark_one_writer();
        // Also wake all readers in case batches are waiting
        self.unpark_all_readers();
    }

    #[cold]
    pub(crate) fn suspend(&self, t: MutexType) -> bool {
        match t {
            MutexType::Exclusive => {
                if self.inner().state.load(Ordering::Relaxed) & WRITERS_PARKED == 0 {
                    return false;
                }
                let (parker, unparker) = ThreadParker::new();
                {
                    let _g = self.inner().wait_mutex.lock();
                    self.inner().writers_waiters.push(unparker);
                }
                parker.park();
                true
            }
            MutexType::Group => {
                if self.inner().state.load(Ordering::Relaxed) & READERS_PARKED == 0 {
                    return false;
                }
                let (parker, unparker) = ThreadParker::new();
                {
                    let _g = self.inner().wait_mutex.lock();
                    self.inner().readers_waiters.push(unparker);
                }
                parker.park();
                true
            }
        }
    }

    #[cold]
    pub(crate) fn pause(&self, timeout: Duration) {
        thread::sleep(timeout)
    }

    #[inline(always)]
    pub(crate) fn wake_all(&self, t: MutexType) {
        match t {
            MutexType::Exclusive => self.unpark_one_writer(), // semantics: wake one writer (exclusive queue is FIFO-ish)
            MutexType::Group => self.unpark_all_readers(),
        }
    }

    #[inline(always)]
    pub(crate) fn wake(&self, t: MutexType) -> bool {
        match t {
            MutexType::Exclusive => {
                self.unpark_one_writer();
                true
            }
            MutexType::Group => {
                self.unpark_all_readers();
                true
            }
        }
    }

    #[cold]
    fn unpark_all_readers(&self) {
        let inner = self.inner();
        // Detach the readers queue under the small wait_mutex to avoid races with suspend
        let to_wake_iter = {
            let _g = inner.wait_mutex.lock();
            let it = inner.readers_waiters.drain();
            // Clear READERS_PARKED flag if queue is empty
            if it.is_none() {
                let _ = inner.state.fetch_and(!READERS_PARKED, Ordering::Relaxed);
            }
            it
        };
        if let Some(iter) = to_wake_iter {
            for u in iter {
                u.unpark();
            }
        }
    }

    #[cold]
    fn unpark_one_writer(&self) {
        let inner = self.inner();
        let one;
        {
            let _g = inner.wait_mutex.lock();
            one = inner.writers_waiters.pop();
            if one.is_none() {
                let _ = inner.state.fetch_and(!WRITERS_PARKED, Ordering::Relaxed);
            }
        }
        if let Some(u) = one {
            u.unpark();
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
        if self.inner().ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            fence(Ordering::Acquire);
            let ptr = self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.inner().state.load(Ordering::Relaxed);
        f.debug_struct("Mutex")
            .field("exclusive", &is_writer_locked(s))
            .field("group", &(reader_count(s) > 0))
            .field("lockers", &reader_count(s))
            .field("ref", &self.get_ref_count())
            .finish()
    }
}
