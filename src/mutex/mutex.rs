use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{
    AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
    fence,
};
use std::thread;

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
    readers: usize,
    // FIXED: Usa wakers invece di writers_waiting/readers_waiting separati
    wakers: usize,
}

struct InnerMutex {
    smutex: SMutex,
    data: UnsafeCell<StateData>,
    cvar_exclusive: SCondVar,
    cvar_group: SCondVar,
    ref_count: AtomicUsize,
    state_atomic: AtomicUsize,
    // FIXED: Counter atomico separato per i reader (come total_g_locked_atomic nel Grutex)
    total_readers_atomic: AtomicUsize,
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
                wakers: 0,
            }),
            cvar_exclusive: SCondVar::new(),
            cvar_group: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
            state_atomic: AtomicUsize::new(UNLOCKED),
            total_readers_atomic: AtomicUsize::new(0),
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

    // FIXED: Usa l'atomic counter per letture lock-free
    pub fn get_readers(&self) -> usize {
        self.inner().total_readers_atomic.load(Acquire)
    }

    pub fn is_locked_group(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        let total = self.inner().total_readers_atomic.load(Acquire);
        st == LOCKED_GROUP || (st == DIRTY && total > 0)
    }

    pub fn is_locked_exclusive(&self) -> bool {
        self.inner().state_atomic.load(Acquire) == LOCKED_EXCLUSIVE
    }

    pub fn is_locked(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        let total = self.inner().total_readers_atomic.load(Acquire);
        st == LOCKED_EXCLUSIVE || st == LOCKED_GROUP || (st == DIRTY && total > 0)
    }

    pub fn lock_exclusive(&self) {
        let mut guard = self.inner().smutex.lock();

        loop {
            {
                let data = self.inner().data(&guard);
                data.wakers = data.wakers.saturating_add(1);

                // FIXED: Controlla sia lo stato che il counter atomico (come nel Grutex)
                let total_readers = self.inner().total_readers_atomic.load(Acquire);
                if data.state == UNLOCKED || (data.state == DIRTY && total_readers == 0) {
                    data.state = LOCKED_EXCLUSIVE;
                    self.inner().state_atomic.store(LOCKED_EXCLUSIVE, Release);
                    data.wakers = data.wakers.saturating_sub(1);
                    break;
                }
            }

            guard = self.inner().cvar_exclusive.wait(guard);
        }
    }

    pub fn lock_group(&self) {
        let mut guard = self.inner().smutex.lock();

        {
            let data = self.inner().data(&guard);
            data.wakers = data.wakers.saturating_add(1);

            // FIXED: Incrementa PRIMA di controllare le condizioni (come nel Grutex)
            data.readers = data.readers.saturating_add(1);
            self.inner().total_readers_atomic.fetch_add(1, AcqRel);
        }

        loop {
            let mut done = false;

            {
                let data = self.inner().data(&guard);

                // FIXED: Consenti group locking quando NON è LOCKED_EXCLUSIVE (come nel Grutex)
                if data.state != LOCKED_EXCLUSIVE {
                    data.state = LOCKED_GROUP;
                    self.inner().state_atomic.store(LOCKED_GROUP, Release);
                    data.wakers = data.wakers.saturating_sub(1);
                    done = true;
                }
            }

            if done {
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

        // FIXED: Aggiorna stato basandoti sul counter atomico (come nel Grutex)
        let total_readers = self.inner().total_readers_atomic.load(Acquire);
        if total_readers == 0 {
            data.state = UNLOCKED;
        } else {
            data.state = LOCKED_GROUP;
        }

        // FIXED: Aggiorna l'atomic DOPO aver cambiato lo stato canonico
        self.inner().state_atomic.store(data.state, Release);

        data.wakers = data.wakers.saturating_sub(1);

        // FIXED: Notifica come nel Grutex - prima i group, poi gli exclusive
        self.inner().cvar_group.notify_all();
        self.inner().cvar_exclusive.notify_one();
    }

    pub fn unlock_group(&self) {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        if data.state != LOCKED_GROUP && data.state != DIRTY {
            panic!("unlock_group called but not locked group");
        }

        // FIXED: Decrementa wakers (come nel Grutex)
        data.wakers = data.wakers.saturating_sub(1);

        if data.readers == 0 {
            panic!("unlock_group called with zero readers");
        }

        data.readers -= 1;
        // FIXED: Aggiorna il counter atomico
        self.inner().total_readers_atomic.fetch_sub(1, AcqRel);

        // FIXED: Aggiorna stato basandoti sul counter atomico DOPO averlo aggiornato
        let total_after = self.inner().total_readers_atomic.load(Acquire);
        if total_after == 0 {
            data.state = DIRTY;  // FIXED: Usa DIRTY invece di UNLOCKED
        } else {
            data.state = LOCKED_GROUP;
        }

        // FIXED: Aggiorna l'atomic DOPO aver cambiato lo stato canonico
        self.inner().state_atomic.store(data.state, Release);

        // FIXED: Notifica sempre entrambi (come nel Grutex)
        self.inner().cvar_exclusive.notify_one();
        self.inner().cvar_group.notify_all();
    }

    pub fn unlock_all_group(&self) {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        if data.state != LOCKED_GROUP && data.state != DIRTY {
            panic!("unlock_all_group called but not locked group");
        }

        // FIXED: Come nel Grutex, aggiorna tutto atomicamente
        let prev_readers = data.readers;
        if prev_readers > 0 {
            data.readers = 0;
            self.inner().total_readers_atomic.store(0, Release);
            data.wakers = data.wakers.saturating_sub(prev_readers);
        }

        data.state = DIRTY;  // FIXED: Usa DIRTY
        self.inner().state_atomic.store(DIRTY, Release);

        // FIXED: Notifica come nel Grutex
        self.inner().cvar_group.notify_all();
        self.inner().cvar_exclusive.notify_one();
    }

    // FIXED: Metodi per test/sospensione migliorati basandosi sul Grutex
    pub(crate) fn suspend(&self, t: MutexType) -> bool {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);

        if data.wakers == 0 {
            return false;
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
            fence(Acquire);  // FIXED: Aggiungi fence come nel Grutex
            let ptr = self.ptr as *mut InnerMutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Mutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.inner().smutex.lock();
        let data = self.inner().data(&guard);
        let total = self.inner().total_readers_atomic.load(Acquire);
        f.debug_struct("Mutex")
            .field("state", &data.state)
            .field("exclusive", &(data.state == LOCKED_EXCLUSIVE))
            .field("group", &(data.state == LOCKED_GROUP || data.state == DIRTY))
            .field("readers", &total)
            .field("ref", &self.get_ref_count())
            .finish()
    }
}