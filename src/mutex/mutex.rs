use crossbeam_queue::SegQueue;
use std::ptr::NonNull;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::thread;
use std::thread::Thread;
use std::time::{Duration, Instant};

/// A fast user space thread locker
/// ```
/// use std::time::Duration;
/// use std::thread::sleep;
/// use std::thread;
/// use castbox::Mutex;
///
/// let mutex = Mutex::new();
///
/// let m1 = mutex.clone();
/// let m2 = mutex.clone();
///
/// let h1 = thread::spawn(move || {
///    m1.lock();
///    sleep(Duration::from_millis(100));
///    m1.unlock();
/// });
///
/// let h2 = thread::spawn(move || {
///     m2.lock();
///     m2.unlock();
/// });
///
/// h1.join().unwrap();
/// h2.join().unwrap();
///
/// drop(mutex);
///```
type State = u8;

const ALLOW_PARKING: bool = true;

const UNLOCKED: State = 0;
const LOCKED: State = 1; // locked, no other threads waiting
const CONTENDED: State = 2; // locked, and other threads waiting (contended)

#[repr(C)]
#[derive(Debug)]
struct InnerMutex {
    state: AtomicU8,
    ref_count: AtomicUsize,
    parked: SegQueue<Thread>,
}

#[repr(transparent)]
#[derive(Debug)]
pub struct Mutex {
    ptr: NonNull<InnerMutex>,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

impl Mutex {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerMutex {
            state: AtomicU8::new(UNLOCKED),
            ref_count: AtomicUsize::new(1),
            parked: SegQueue::new(),
        }));
        Self {
            ptr: NonNull::new(ptr).expect("Happened an invalid allocation for Mutex"),
        }
    }

    #[inline]
    fn inner(&self) -> &InnerMutex {
        unsafe { &*self.ptr.as_ptr() }
    }

    #[inline]
    pub fn try_lock(&self) -> bool {
        self.inner()
            .state
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_ok()
    }

    #[inline]
    pub fn lock(&self) {
        if self
            .inner()
            .state
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_err()
        {
            self.lock_contended();
        }
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.inner().state.load(Relaxed) != UNLOCKED
    }

    fn lock_contended(&self) {
        // Spin first to speed things up if the lock is released quickly.
        let mut state = self.spin(100);

        // If it's unlocked now, attempt to take the lock
        // without marking it as contended.
        if state == UNLOCKED {
            match self
                .inner()
                .state
                .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            {
                Ok(_) => return, // Locked!
                Err(s) => state = s,
            }
        }

        loop {
            // Put the lock in contended state.
            // We avoid an unnecessary write if it as already set to CONTENDED,
            // to be friendlier for the caches.
            if state != CONTENDED && self.inner().state.swap(CONTENDED, Acquire) == UNLOCKED {
                // We changed it from UNLOCKED to CONTENDED, so we just successfully locked it.
                return;
            }

            // Wait for the futex to change state, assuming it is still CONTENDED.
            self.us_wait(&self.inner().state, CONTENDED, None);

            // Spin again after waking up.
            state = self.spin(100);
        }
    }

    fn spin(&self, mut spin: i32) -> State {
        loop {
            // We only use `load` (and not `swap` or `compare_exchange`)
            // while spinning, to be easier on the caches.
            let state = self.inner().state.load(Relaxed);

            // We stop spinning when the mutex is UNLOCKED,
            // but also when it's CONTENDED.
            if state != LOCKED || spin == 0 {
                return state;
            }

            std::hint::spin_loop();
            spin -= 1;
        }
    }

    #[inline]
    pub fn unlock(&self) {
        if self.inner().state.swap(UNLOCKED, Release) == CONTENDED && ALLOW_PARKING {
            // We only wake up one thread. When that thread locks the mutex, it
            // will mark the mutex as CONTENDED (see lock_contended above),
            // which makes sure that any other waiting threads will also be
            // woken up eventually.
            self.wake();
        }
    }

    #[inline(always)]
    fn suspend(&self) {
        unsafe {
            self.ptr.as_ref().parked.push(thread::current());
        };
        thread::park()
    }

    #[inline(always)]
    fn wake(&self) {
        let thread = unsafe { &self.ptr.as_ref().parked };
        if let Some(thread) = thread.pop() {
            thread.unpark();
        }
    }

    fn us_wait(&self, state: &AtomicU8, expected: u8, timeout: Option<Duration>) {
        let mut backoff = 1;

        if let Some(max_dur) = timeout {
            let start = Instant::now();

            while state.load(Acquire) == expected {
                if start.elapsed() >= max_dur {
                    break;
                }
                backoff = self.cpu_relax(backoff, false);
            }
        } else {
            while state.load(Acquire) == expected {
                backoff = self.cpu_relax(backoff, ALLOW_PARKING);
            }
        }
    }

    #[inline(always)]
    fn cpu_relax(&self, backoff: i32, park: bool) -> i32 {
        if backoff <= 64 {
            std::hint::spin_loop();
        } else {
            if park {
                self.suspend();
                return 1;
            } else {
                if backoff <= 512 {
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_micros(backoff as u64));
                }
            }
        }

        (backoff * 2).min(10_000)
    }
}

impl Clone for Mutex {
    fn clone(&self) -> Self {
        unsafe {
            self.ptr.as_ref().ref_count.fetch_add(1, Acquire);
        }
        Mutex { ptr: self.ptr }
    }
}

impl Drop for Mutex {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Release) == 1 {
            atomic::fence(Release);

            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
}
