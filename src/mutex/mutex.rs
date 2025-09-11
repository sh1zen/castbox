use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering, fence};
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

        let d = self.inner().data(&guard);
        d.wakers = d.wakers.saturating_add(1);

        loop {
            let d = self.inner().data(&guard);
            if d.state == UNLOCKED || (d.state == DIRTY && d.g_locked == 0) {
                d.state = LOCKED_EXCLUSIVE;
                d.wakers = d.wakers.saturating_sub(1);
                break;
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


#[cfg(test)]
mod tests_mutex {
    use crate::mutex::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn kiko() {
        let m = Mutex::new();

        m.lock_exclusive();

        let mc = m.clone();
        let t1 = thread::spawn(move || {
            mc.lock_group();
            mc.lock_group();
            mc.lock_group();

            thread::sleep(Duration::from_millis(200));
            mc.unlock_all_group();
        });

        let mc = m.clone();
        let t2 = thread::spawn(move || {
            let _x = mc.clone();
            mc.unlock_exclusive();
            thread::sleep(Duration::from_millis(100));
            mc.lock_exclusive();
        });

        t1.join().unwrap();
        t2.join().unwrap();

        m.unlock_exclusive();
    }

    #[test]
    fn stress_test() {
        let mut handles = vec![];

        let mutex = Mutex::new();

        mutex.lock_exclusive();

        for _i in 0..100 {
            let m1 = mutex.clone();
            handles.push(thread::spawn(move || {
                m1.lock_group();
            }));
        }

        assert!(!mutex.is_locked_group());

        mutex.unlock_exclusive();

        for h in handles {
            h.join().unwrap();
        }

        assert!(mutex.is_locked_group());
        drop(mutex);
    }

    #[test]
    fn test_mutex() {
        use crate::mutex::Mutex;
        use std::thread;
        use std::thread::sleep;
        use std::time::Duration;

        let mutex = Mutex::new();

        let m1 = mutex.clone();
        let m2 = mutex.clone();

        mutex.lock_group();
        mutex.lock_group();

        mutex.unlock_group();

        let h1 = thread::spawn(move || {
            m1.lock_exclusive();
            sleep(Duration::from_millis(100));
            m1.unlock_exclusive();
        });

        let h2 = thread::spawn(move || {
            m2.lock_exclusive();
            m2.unlock_exclusive();
        });

        mutex.unlock_group();

        h1.join().unwrap();
        h2.join().unwrap();

        drop(mutex);
    }

    #[test]
    fn refcount_clone_drop() {
        let m = Mutex::new();
        assert_eq!(m.get_ref_count(), 1);
        let c1 = m.clone();
        let c2 = m.clone();
        assert_eq!(m.get_ref_count(), 3);
        drop(c1);
        drop(c2);
        assert_eq!(m.get_ref_count(), 1);
    }

    #[test]
    fn is_locked_reflects_state() {
        let m = Mutex::new();
        assert!(!m.is_locked_exclusive());
        {
            let _g = m.lock_exclusive();
            assert!(m.is_locked_exclusive());
            m.unlock_exclusive();
        }
        assert!(!m.is_locked_exclusive());
    }

    #[test]
    fn exclusive_blocks_others() {
        let m = Mutex::new();

        let entered_group = Arc::new(AtomicBool::new(false));
        let entered_excl = Arc::new(AtomicBool::new(false));

        m.lock_exclusive();
        let eg = entered_group.clone();
        let mg = m.clone();
        let tg = thread::spawn(move || {
            mg.lock_group();
            eg.store(true, Ordering::Release);
            mg.unlock_group();
        });

        let ee = entered_excl.clone();
        let me = m.clone();
        let te = thread::spawn(move || {
            me.lock_exclusive();
            ee.store(true, Ordering::Release);
            me.unlock_exclusive();
        });

        thread::sleep(Duration::from_millis(50));
        assert!(!entered_group.load(Ordering::Acquire));
        assert!(!entered_excl.load(Ordering::Acquire));

        m.unlock_exclusive();

        tg.join().unwrap();
        te.join().unwrap();

        assert!(entered_group.load(Ordering::Acquire));
        assert!(entered_excl.load(Ordering::Acquire));
    }

    #[test]
    fn group_allows_concurrency() {
        let m = Mutex::new();
        const N: usize = 6;

        let barrier = Arc::new(Barrier::new(N));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut ths = Vec::new();
        for _ in 0..N {
            let mm = m.clone();
            let b = barrier.clone();
            let cur = concurrent.clone();
            let maxc = max_concurrent.clone();
            ths.push(thread::spawn(move || {
                mm.lock_group();
                b.wait();
                let now = cur.fetch_add(1, Ordering::AcqRel) + 1;
                maxc.fetch_max(now, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(20));
                cur.fetch_sub(1, Ordering::AcqRel);
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        m.unlock_all_group();
        assert!(max_concurrent.load(Ordering::Acquire) > 1);
        assert!(!m.is_locked_exclusive());
    }

    #[test]
    fn exclusives_are_mutually_exclusive() {
        let m = Mutex::new();
        let inside = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(true));

        let mut ths = Vec::new();
        for _i in 0..4 {
            let mm = m.clone();
            let inside = inside.clone();
            let ok = ok.clone();
            ths.push(thread::spawn(move || {
                let _x = _i;
                let _k = mm.get_ref_count();
                for _j in 0..50 {
                    mm.lock_exclusive();
                    if inside.swap(true, Ordering::AcqRel) {
                        ok.store(false, Ordering::Release);
                    }
                    //thread::sleep(Duration::from_millis(1));
                    inside.store(false, Ordering::Release);
                    mm.unlock_exclusive();
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        assert!(ok.load(Ordering::Acquire));
    }

    #[test]
    fn group_batch_then_exclusive() {
        let m = Mutex::new();
        const G: usize = 4;
        let barrier_in = Arc::new(Barrier::new(G));
        let barrier_out = Arc::new(Barrier::new(G));

        let mut tg = Vec::new();
        for _ in 0..G {
            let mm = m.clone();
            let bin = barrier_in.clone();
            let bout = barrier_out.clone();
            tg.push(thread::spawn(move || {
                mm.lock_group();
                bin.wait();
                thread::sleep(Duration::from_millis(30));
                bout.wait();
                mm.unlock_group();
            }));
        }

        let entered_excl = Arc::new(AtomicBool::new(false));
        let ee = entered_excl.clone();
        let me = m.clone();
        let te = thread::spawn(move || {
            me.lock_exclusive();
            ee.store(true, Ordering::Release);
            me.unlock_exclusive();
        });

        te.join().unwrap();
        for t in tg {
            t.join().unwrap();
        }

        assert!(entered_excl.load(Ordering::Acquire));
    }

    #[test]
    fn unlock_panics_if_group_locked() {
        let m = Mutex::new();
        m.lock_group();
        let res = std::panic::catch_unwind(|| {
            m.unlock_exclusive();
        });
        assert!(res.is_err());
        m.unlock_group();
    }

    #[test]
    fn stress_multi_lock() {
        let m = Mutex::new();

        let mut ths = Vec::new();
        for id in 0..8 {
            let mm = m.clone();
            ths.push(thread::spawn(move || {
                for i in 0..100 {
                    if (id + i) % 3 == 0 {
                        mm.lock_exclusive();
                        mm.unlock_exclusive();
                    } else {
                        mm.lock_group();
                        mm.unlock_group();
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
    }
}
