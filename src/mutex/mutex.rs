use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{fence, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

pub(crate) enum MutexType {
    Exclusive,
    Group,
}

type State = usize;

const UNLOCKED: State = 0;
const LOCKED_EXCLUSIVE: State = 1;
const LOCKED_GROUP: State = 2;
const DIRTY: State = 3;

struct StateData {
    state: State,
    wakers: usize,
    g_locked: usize,
}

struct InnerMutex {
    state_mutex: SMutex,
    data: UnsafeCell<StateData>,
    cvar_exclusive: SCondVar,
    cvar_group: SCondVar,
    ref_count: AtomicUsize,
}

unsafe impl Send for InnerMutex {}
unsafe impl Sync for InnerMutex {}

impl InnerMutex {
    fn new() -> Self {
        Self {
            state_mutex: SMutex::new(),
            data: UnsafeCell::new(StateData {
                state: UNLOCKED,
                wakers: 0,
                g_locked: 0,
            }),
            cvar_exclusive: SCondVar::new(),
            cvar_group: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
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
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mutex");
        }
        Self { ptr }
    }

    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Ordering::Acquire)
    }

    pub fn get_group_locked(&self) -> usize {
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        d.g_locked
    }

    pub fn is_locked_group(&self) -> bool {
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        d.state == LOCKED_GROUP || (d.state == DIRTY && d.g_locked > 0)
    }

    pub fn is_locked_exclusive(&self) -> bool {
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        d.state == LOCKED_EXCLUSIVE
    }

    pub fn is_locked(&self) -> bool {
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        d.state == LOCKED_EXCLUSIVE
            || d.state == LOCKED_GROUP
            || (d.state == DIRTY && d.g_locked > 0)
    }

    fn spin(&self, _spin: usize) -> State {
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        d.state
    }
    
    pub fn lock_exclusive(&self) {
        let mut guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            d.wakers = d.wakers.saturating_add(1);
        }

        loop {
            {
                let d = self.inner().data(&guard);
                if d.state == UNLOCKED || (d.state == DIRTY && d.g_locked == 0) {
                    d.state = LOCKED_EXCLUSIVE;
                    d.wakers = d.wakers.saturating_sub(1);
                    break;
                }
            }

            guard = self.inner().cvar_exclusive.wait(guard);
        }
    }

    pub fn lock_group(&self) {
        let mut guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            d.wakers = d.wakers.saturating_add(1);
            d.g_locked = d.g_locked.saturating_add(1);
        }

        loop {
            {
                let d = self.inner().data(&guard);
                if d.state == UNLOCKED || d.state == LOCKED_GROUP || d.state == DIRTY {
                    if d.state == UNLOCKED || d.state == DIRTY {
                        d.state = LOCKED_GROUP;
                        self.inner().cvar_group.notify_all();
                    }
                    let d = self.inner().data(&guard);
                    d.wakers = d.wakers.saturating_sub(1);
                    break;
                }
            }

            guard = self.inner().cvar_group.wait(guard);
        }
    }

    pub fn unlock_group(&self) {
        let guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            if d.state != LOCKED_GROUP && d.state != DIRTY {
                panic!("Trying to unlock a non Locked Group {}", d.state);
            }

            d.wakers = d.wakers.saturating_sub(1);
            d.g_locked = d.g_locked.saturating_sub(1);

            if d.g_locked == 0 {
                d.state = DIRTY;

                self.inner().cvar_exclusive.notify_one();
                self.inner().cvar_group.notify_all();
            }
        }
    }

    pub fn unlock_exclusive(&self) {
        let guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            if d.state != LOCKED_EXCLUSIVE {
                panic!("Is not Locked Exclusive (state = {})", d.state);
            }

            d.state = UNLOCKED;
            d.wakers = d.wakers.saturating_sub(1);

            self.inner().cvar_group.notify_all();
            self.inner().cvar_exclusive.notify_one();
        }
    }

    pub fn unlock_all_group(&self) {
        let guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            if d.state != LOCKED_GROUP && d.state != DIRTY {
                panic!("Trying to unlock a non Locked Group {}", d.state);
            }

            let prev = d.g_locked;
            d.g_locked = 0;
            if d.state == LOCKED_GROUP {
                d.state = DIRTY;
            }
            d.wakers = d.wakers.saturating_sub(prev);
        }

        self.inner().cvar_exclusive.notify_one();
        self.inner().cvar_group.notify_all();
    }

    pub(crate) fn suspend(&self, t: MutexType) -> bool {
        let guard = self.inner().state_mutex.lock();
        {
            let d = self.inner().data(&guard);
            if d.wakers == 0 {
                return false;
            }
        }

        match t {
            MutexType::Exclusive => {
                let _ = self.inner().cvar_exclusive.wait(guard);
            }
            MutexType::Group => {
                let _ = self.inner().cvar_group.wait(guard);
            }
        }

        true
    }

    pub(crate) fn pause(&self, timeout: Duration) {
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
        let guard = self.inner().state_mutex.lock();
        let d = self.inner().data(&guard);
        f.debug_struct("Mutex")
            .field("exclusive", &(d.state == LOCKED_EXCLUSIVE))
            .field("group", &(d.state == LOCKED_GROUP || d.state == DIRTY))
            .field("lockers", &d.g_locked)
            .field("ref", &self.get_ref_count())
            .finish()
    }
}
