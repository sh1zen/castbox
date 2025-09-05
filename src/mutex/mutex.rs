use crate::collections::AtomicVec;
use crate::mutex::thread::ThreadParker;
use crate::mutex::Backoff;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::time::Duration;
use std::{fmt, hint, thread};

pub(crate) enum MutexType {
    Exclusive,
    Group,
}

/// A fast user space thread locker
type State = usize;

/// unlocked
const UNLOCKED: State = 0;
/// locked, no other threads waiting
const LOCKED_EXCLUSIVE: State = 1;
/// multi group lock
const LOCKED_GROUP: State = 2;
/// a dirty state
const DIRTY: State = 3;

struct InnerMutex {
    state: AtomicUsize,
    ref_count: AtomicUsize,
    parking_e: AtomicVec<ThreadParker>,
    parking_g: AtomicVec<ThreadParker>,
    g_locked: AtomicUsize,
    wakers: AtomicUsize,
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
            state: AtomicUsize::new(UNLOCKED),
            ref_count: AtomicUsize::new(1),
            parking_e: AtomicVec::new(),
            parking_g: AtomicVec::new(),
            g_locked: AtomicUsize::new(0),
            wakers: AtomicUsize::new(0),
        }));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mutex");
        }
        Self { ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    pub fn get_group_locked(&self) -> usize {
        self.inner().g_locked.load(Acquire)
    }

    #[inline(always)]
    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr }
    }

    pub fn lock_exclusive(&self) {
        let backoff = Backoff::new();
        let inner = self.inner();

        inner.wakers.fetch_add(1, Release);

        loop {
            // Spin first to speed things up if the lock is Relaxed quickly.
            match self.spin(10) {
                DIRTY => {
                    // if the state is DIRTY and there are no other group waiting is safe to switch to LOCKED
                    if inner.g_locked.load(Acquire) == 0 {
                        if inner
                            .state
                            .compare_exchange(DIRTY, LOCKED_EXCLUSIVE, Acquire, Relaxed)
                            .is_ok()
                        {
                            break;
                        }
                    } else {
                        // todo we have many running group locks so this thread can sleep
                        //if self.suspend(MutexType::Exclusive) {
                        //     backoff.reset();
                        // }
                        continue;
                    }
                }
                _ => {
                    if inner
                        .state
                        .compare_exchange(UNLOCKED, LOCKED_EXCLUSIVE, Acquire, Relaxed)
                        .is_ok()
                    {
                        break;
                    }
                }
            }

            if backoff.is_completed() {
                if self.suspend(MutexType::Exclusive) {
                    backoff.reset();
                } else {
                    hint::spin_loop();
                }
            } else {
                backoff.snooze();
            }
        }
    }

    pub fn lock_group(&self) {
        let inner = self.inner();
        let backoff = Backoff::new();

        // segnala che c’è un thread in competizione
        inner.wakers.fetch_add(1, Release);

        // we add it here so that as soon as the lock is available we can proceed to execute
        // all the multi lock group.
        // SAFETY: The unlock will fetch_sub only when the internal state is on LOCKED_GROUP state
        inner.g_locked.fetch_add(1, Release);

        loop {
            // Spin first to speed things up if the lock is Relaxed quickly.
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
                    // fix some data race
                    let _ = inner.state.load(Acquire);
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
                if self.suspend(MutexType::Group) {
                    backoff.reset();
                } else {
                    hint::spin_loop();
                }
            } else {
                backoff.snooze();
            }
        }
    }

    #[inline]
    pub fn is_locked_group(&self) -> bool {
        let state = self.inner().state.load(Acquire);
        state == LOCKED_GROUP || (state == DIRTY && self.inner().g_locked.load(Acquire) > 0)
    }

    #[inline]
    pub fn is_locked_exclusive(&self) -> bool {
        self.inner().state.load(Acquire) == LOCKED_EXCLUSIVE
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.is_locked_exclusive() || self.is_locked_group()
    }

    #[inline]
    fn spin(&self, mut spin: usize) -> State {
        let inner = self.inner();
        loop {
            // We only use `load` (and not `swap` or `compare_exchange`)
            // while spinning, to be easier on the caches.
            let state = inner.state.load(Relaxed);

            // We stop spinning when the mutex is UNLOCKED
            if state == UNLOCKED || state == DIRTY || spin == 0 {
                return state;
            }

            hint::spin_loop();
            spin -= 1;
        }
    }

    pub fn unlock_all_group(&self) {
        let inner = self.inner();
        let state = inner.state.load(Acquire);

        if state != LOCKED_GROUP && state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", state);
        }

        let prev = self.inner().g_locked.swap(0, AcqRel);

        if prev > 0 {
            inner.wakers.fetch_sub(prev, Release);
        }

        // if we were already in a DIRTY state don't do anything
        if state == LOCKED_GROUP {
            let _ = inner
                .state
                .compare_exchange(LOCKED_GROUP, DIRTY, Acquire, Relaxed);
        }

        // if there are some thread suspended now we must wake them up
        if !self.wake(MutexType::Exclusive) {
            self.wake(MutexType::Group);
        }
    }

    pub fn unlock_group(&self) {
        let inner = self.inner();
        let state = inner.state.load(Acquire);

        if state != LOCKED_GROUP && state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", state);
        }

        inner.wakers.fetch_sub(1, Release);

        if inner.g_locked.fetch_sub(1, AcqRel) == 1 {
            let _ = inner.state.compare_exchange(state, DIRTY, Acquire, Relaxed);

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
            .compare_exchange(LOCKED_EXCLUSIVE, UNLOCKED, Acquire, Relaxed)
            .is_err()
        {
            panic!(
                "Is not Locked or is a Locked Group {}",
                self.inner().state.load(Acquire)
            );
        }

        self.inner().wakers.fetch_sub(1, Release);

        // if there are some thread suspended now we must wake them up
        if !self.wake(MutexType::Group) {
            self.wake(MutexType::Exclusive);
        }
    }

    #[inline]
    pub(crate) fn suspend(&self, t: MutexType) -> bool {
        let inner = self.inner();
        let mut res = false;

        // We are allowed to park current thread if there are at least a total of 2 thread running
        if inner.wakers.fetch_sub(1, AcqRel) > 1 {
            let parking = match t {
                MutexType::Exclusive => &inner.parking_e,
                MutexType::Group => &inner.parking_g,
            };

            let tp = ThreadParker::new();
            parking.push(tp.clone());

            // now that we have inserted the thread in the queue
            // we can park if there is at least another thread running
            if inner.wakers.load(Acquire) > 0 {
                tp.park();
                res = true;
            }
        }

        inner.wakers.fetch_add(1, Release);
        res
    }

    #[inline]
    pub(crate) fn pause(&self, timeout: Duration) {
        thread::sleep(timeout)
    }

    #[inline]
    pub(crate) fn wake_all(&self, t: MutexType) {
        let parking = match t {
            MutexType::Exclusive => &self.inner().parking_e,
            MutexType::Group => &self.inner().parking_g,
        };

        while let Some(thread) = parking.pop() {
            thread.unpark();
        }
    }

    #[inline]
    pub(crate) fn wake(&self, t: MutexType) -> bool {
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
        if self.inner().ref_count.fetch_sub(1, AcqRel) == 1 {
            atomic::fence(Acquire);

            let ptr = self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex")
            .field("exclusive", &self.is_locked_exclusive())
            .field("group", &self.is_locked_group())
            .field("lockers", &self.get_group_locked())
            .field("ref", &self.get_ref_count())
            .finish()
    }
}
