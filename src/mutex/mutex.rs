use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering::{Acquire, Release, Relaxed}};
use std::thread;

pub(crate) enum MutexType {
    Exclusive,
    Group,
}

type State = usize;
const UNLOCKED: State = 0;
const LOCKED_EXCLUSIVE: State = 1;
const LOCKED_GROUP: State = 2;

struct StateData {
    state: State,
    readers: usize,
    writers_waiting: usize,
    readers_waiting: usize,
}

struct InnerMutex {
    smutex: SMutex,
    data: UnsafeCell<StateData>,
    cvar_exclusive: SCondVar,
    cvar_group: SCondVar,
    ref_count: AtomicUsize,
    state_atomic: AtomicUsize,
}

unsafe impl Send for InnerMutex {}
unsafe impl Sync for InnerMutex {}

impl InnerMutex {
    fn new() -> Self {
        Self {
            smutex: SMutex::new(),
            data: UnsafeCell::new(StateData {
                state: UNLOCKED,
                readers: 0,
                writers_waiting: 0,
                readers_waiting: 0,
            }),
            cvar_exclusive: SCondVar::new(),
            cvar_group: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
            state_atomic: AtomicUsize::new(UNLOCKED),
        }
    }

    #[inline]
    fn data<'a>(&'a self, _guard: &'a SGuard<'a>) -> &'a mut StateData {
        unsafe { &mut *self.data.get() }
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
        Self { ptr }
    }

    #[inline]
    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    pub fn is_locked_group(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        st == LOCKED_GROUP
    }

    pub fn is_locked_exclusive(&self) -> bool {
        self.inner().state_atomic.load(Acquire) == LOCKED_EXCLUSIVE
    }

    pub fn is_locked(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        st == LOCKED_EXCLUSIVE || st == LOCKED_GROUP
    }

    pub fn lock_exclusive(&self) {
        let mut guard = self.inner().smutex.lock();

        self.inner().data(&guard).writers_waiting += 1;
        loop {
            let data = self.inner().data(&guard);
            if data.state == UNLOCKED {
                data.state = LOCKED_EXCLUSIVE;
                self.inner().state_atomic.store(LOCKED_EXCLUSIVE, Release);
                data.writers_waiting -= 1;
                break;
            }
            guard = self.inner().cvar_exclusive.wait(guard);
        }
    }

    pub fn lock_group(&self) {
        let mut guard = self.inner().smutex.lock();

        self.inner().data(&guard).readers_waiting += 1;
        loop {
            let data = self.inner().data(&guard);
            if data.state != LOCKED_EXCLUSIVE && data.writers_waiting == 0 {
                data.readers += 1;
                data.state = LOCKED_GROUP;
                self.inner().state_atomic.store(LOCKED_GROUP, Release);
                data.readers_waiting -= 1;
                break;
            }
            guard = self.inner().cvar_group.wait(guard);
        }
    }

    pub fn unlock_exclusive(&self) {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        if data.state != LOCKED_EXCLUSIVE {
            panic!("unlock_exclusive called but not locked exclusive");
        }

        data.state = UNLOCKED;
        self.inner().state_atomic.store(UNLOCKED, Release);

        // Sveglia writer prima se presenti
        if data.writers_waiting > 0 {
            self.inner().cvar_exclusive.notify_one();
        } else if data.readers_waiting > 0 {
            self.inner().cvar_group.notify_all();
        }
    }

    pub fn unlock_group(&self) {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        if data.readers == 0 {
            panic!("unlock_group called with zero readers");
        }

        data.readers -= 1;

        if data.readers == 0 {
            data.state = UNLOCKED;
            self.inner().state_atomic.store(UNLOCKED, Release);

            if data.writers_waiting > 0 {
                self.inner().cvar_exclusive.notify_one();
            } else if data.readers_waiting > 0 {
                self.inner().cvar_group.notify_all();
            }
        } else {
            data.state = LOCKED_GROUP;
            self.inner().state_atomic.store(LOCKED_GROUP, Release);
            self.inner().cvar_group.notify_all();
        }
    }

    pub fn unlock_all_group(&self) {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        data.readers = 0;
        data.state = UNLOCKED;
        self.inner().state_atomic.store(UNLOCKED, Release);

        if data.writers_waiting > 0 {
            self.inner().cvar_exclusive.notify_one();
        } else if data.readers_waiting > 0 {
            self.inner().cvar_group.notify_all();
        }
    }

    // Metodi per test/sospensione
    pub(crate) fn suspend(&self, t: MutexType) -> bool {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        match t {
            MutexType::Exclusive => {
                if data.writers_waiting == 0 {
                    return false;
                }
                let _ = self.inner().cvar_exclusive.wait(guard);
            }
            MutexType::Group => {
                if data.readers_waiting == 0 {
                    return false;
                }
                let _ = self.inner().cvar_group.wait(guard);
            }
        }
        true
    }

    pub(crate) fn pause(&self, timeout: std::time::Duration) {
        thread::sleep(timeout)
    }

    pub(crate) fn wake_all(&self, t: MutexType) {
        match t {
            MutexType::Exclusive => self.inner().cvar_exclusive.notify_all(),
            MutexType::Group => self.inner().cvar_group.notify_all(),
        }
    }

    pub(crate) fn wake(&self, t: MutexType) -> bool {
        match t {
            MutexType::Exclusive => {
                self.inner().cvar_exclusive.notify_one();
                true
            }
            MutexType::Group => {
                self.inner().cvar_group.notify_one();
                true
            }
        }
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
            let ptr = self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);
        f.debug_struct("Mutex")
            .field("exclusive", &(data.state == LOCKED_EXCLUSIVE))
            .field("group", &(data.readers > 0))
            .field("readers", &data.readers)
            .field("ref", &self.get_ref_count())
            .finish()
    }
}
