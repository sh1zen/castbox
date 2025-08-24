use crate::collections::AtomicVec;
use crate::mutex::Backoff;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::thread::Thread;
use std::{fmt, hint, thread};

enum MutexType {
    Exclusive,
    Group,
}

/// Layout packing (AtomicUsize):
/// lower 3 bit: state
/// remaining bits: locked counter (shifted right by SHIFT_LOCKED)
const SHIFT_LOCKED: usize = 3;
const MASK_STATE: usize = (1 << SHIFT_LOCKED) - 1;

/// A fast user space thread locker
type State = u8;

/// unlocked
const UNLOCKED: State = 0;
/// locked, no other threads waiting
const LOCKED: State = 1;
/// multi group lock
const LOCKED_GROUP: State = 3;
/// a dirty state
const DIRTY: State = 4;

struct InnerMutex {
    state: AtomicU8,
    ref_count: AtomicUsize,
    parking_e: AtomicVec<Thread>,
    parking_g: AtomicVec<Thread>,
    locked: AtomicUsize,
    wake_deadlock: AtomicU8,
}

#[repr(transparent)]
pub struct Mutex {
    ptr: *const InnerMutex,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

impl UnwindSafe for Mutex {}
impl RefUnwindSafe for Mutex {}

impl Mutex {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerMutex {
            state: AtomicU8::new(UNLOCKED),
            ref_count: AtomicUsize::new(1),
            parking_e: AtomicVec::new(),
            parking_g: AtomicVec::new(),
            locked: AtomicUsize::new(0),
            wake_deadlock: AtomicU8::new(UNLOCKED),
        }));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mutex");
        }
        Self { ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    #[inline(always)]
    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr }
    }

    pub fn lock_exclusive(&self) {
        let backoff = Backoff::new();
        let inner = self.inner();

        loop {
            // Spin first to speed things up if the lock is released quickly.
            match self.spin(10) {
                DIRTY => {
                    // if the state is DIRTY and there are no other group waiting is safe to switch to LOCKED
                    if inner.locked.load(Acquire) == 0 {
                        if inner
                            .state
                            .compare_exchange(DIRTY, LOCKED, Acquire, Relaxed)
                            .is_ok()
                        {
                            break;
                        }
                    }
                }
                _ => {
                    if inner
                        .state
                        .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
                        .is_ok()
                    {
                        break;
                    }
                }
            }

            if backoff.is_completed() {
                self.suspend(MutexType::Exclusive);
            } else {
                backoff.snooze();
            }
        }
    }

    pub fn lock_group(&self) {
        let inner = self.inner();
        let backoff = Backoff::new();

        // we add it here so that as soon as the lock is available we can proceed to execute
        // all the multi lock group.
        // SAFETY: The unlock will fetch_sub only when the internal state is on LOCKED_GROUP state
        inner.locked.fetch_add(1, Release);

        loop {
            // Spin first to speed things up if the lock is released quickly.
            match self.spin(10) {
                DIRTY => {
                    if inner
                        .state
                        .compare_exchange(DIRTY, LOCKED_GROUP, Acquire, Relaxed)
                        .is_ok()
                    {
                        self.wake_all(MutexType::Group);
                        break;
                    }
                }
                LOCKED_GROUP => {
                    // fix data race
                    if inner.state.load(Acquire) == LOCKED_GROUP {
                        // if some thread are parked let's wake them up
                        self.wake(MutexType::Group);
                    }
                    break;
                }
                _ => {
                    if inner
                        .state
                        .compare_exchange(UNLOCKED, LOCKED_GROUP, Acquire, Relaxed)
                        .is_ok()
                    {
                        // try to wake some thread that maybe are parked but are members of this group
                        self.wake_all(MutexType::Group);
                        break;
                    }
                }
            }

            if backoff.is_completed() {
                self.suspend(MutexType::Group);
            } else {
                backoff.snooze();
            }
        }
    }

    #[inline]
    pub fn is_locked_group(&self) -> bool {
        let state = self.inner().state.load(Acquire);
        state == LOCKED_GROUP || (state == DIRTY && self.inner().locked.load(Acquire) > 0)
    }

    #[inline]
    pub fn is_locked_exclusive(&self) -> bool {
        let state = self.inner().state.load(Acquire);
        !(state == UNLOCKED || (state == DIRTY && self.inner().locked.load(Acquire) == 0))
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.is_locked_group() || self.is_locked_exclusive()
    }

    #[inline]
    fn spin(&self, mut spin: i32) -> State {
        loop {
            // We only use `load` (and not `swap` or `compare_exchange`)
            // while spinning, to be easier on the caches.
            let state = self.inner().state.load(Relaxed);

            // We stop spinning when the mutex is UNLOCKED
            if state == UNLOCKED || state == DIRTY || spin == 0 {
                return state;
            }

            hint::spin_loop();
            spin -= 1;
        }
    }

    pub fn unlock_all_group(&self) {
        self.inner().locked.store(1, Release);
        self.unlock_group();
    }

    pub fn unlock_group(&self) {
        let inner = self.inner();
        let state = inner.state.load(Acquire);

        if state != LOCKED_GROUP && state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", state);
        }

        if inner.locked.fetch_sub(1, Release) == 1 {
            inner.state.store(DIRTY, Release);

            // if there are some thread suspended now we must wake them up
            if !self.wake(MutexType::Exclusive) {
                self.wake(MutexType::Group);
            }
        }
    }

    pub fn unlock_exclusive(&self) {
        if self
            .inner()
            .state
            .compare_exchange(LOCKED, UNLOCKED, Release, Relaxed)
            .is_err()
        {
            panic!("Is not Locked or is a Locked Group.");
        }

        // if there are some thread suspended now we must wake them up
        if !self.wake(MutexType::Group) {
            self.wake(MutexType::Exclusive);
        }
    }

    pub fn try_lock_exclusive(&self) -> bool {
        if self.inner().locked.load(Acquire) == 0 {
            return self
                .inner()
                .state
                .compare_exchange(DIRTY, LOCKED, Acquire, Relaxed)
                .is_ok();
        }

        self.inner()
            .state
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_ok()
    }

    #[inline]
    fn suspend(&self, t: MutexType) {
        if self
            .inner()
            .wake_deadlock
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_err()
        {
            return;
        }
        let parking = match t {
            MutexType::Exclusive => &self.inner().parking_e,
            MutexType::Group => &self.inner().parking_g,
        };
        parking.push(thread::current());
        self.inner().wake_deadlock.store(UNLOCKED, Release);
        thread::park();
    }

    #[inline]
    fn wake_all(&self, t: MutexType) {
        while self
            .inner()
            .wake_deadlock
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
        let parking = match t {
            MutexType::Exclusive => &self.inner().parking_e,
            MutexType::Group => &self.inner().parking_g,
        };

        if let Some(thread) = parking.pop() {
            thread.unpark();
            // pre-release to improve performances
            self.inner().wake_deadlock.store(UNLOCKED, Release);
            while let Some(thread) = parking.pop() {
                thread.unpark();
            }
        }
        self.inner().wake_deadlock.store(UNLOCKED, Release);
    }

    #[inline]
    fn wake(&self, t: MutexType) -> bool {
        while self
            .inner()
            .wake_deadlock
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
        let parking = match t {
            MutexType::Exclusive => &self.inner().parking_e,
            MutexType::Group => &self.inner().parking_g,
        };
        let res = if let Some(thread) = parking.pop() {
            thread.unpark();
            true
        } else {
            false
        };
        self.inner().wake_deadlock.store(UNLOCKED, Release);
        res
    }
}

impl Clone for Mutex {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Relaxed);
        Mutex { ptr: self.ptr }
    }
}

impl Drop for Mutex {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);

            let ptr = self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner();
        f.debug_struct("Mutex")
            .field("locked", &(inner.state.load(Relaxed) != UNLOCKED))
            .field("group", &inner.state.load(Relaxed))
            .field("lockers", &inner.locked.load(Relaxed))
            .field("ref", &inner.ref_count.load(Relaxed))
            .finish()
    }
}