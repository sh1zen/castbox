use crate::core::futex::{futex_wait, futex_wake};
use crossbeam_utils::CachePadded;
use std::sync::atomic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, Thread};

type State = usize;

const PARKED: State = 0;
const UNPARKED: State = 1;

struct ThreadInner {
    tokens: CachePadded<AtomicUsize>,
    ref_count: CachePadded<AtomicUsize>,
    thread: Thread,
}

impl ThreadInner {
    fn new() -> Self {
        Self {
            tokens: CachePadded::new(AtomicUsize::new(UNPARKED)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
            thread: thread::current(),
        }
    }

    #[inline]
    fn try_consume_token(&self) -> bool {
        self.tokens
            .fetch_update(Ordering::Acquire, Ordering::Acquire, |v| {
                if v > PARKED { Some(v - 1) } else { None }
            })
            .is_ok()
    }

    #[inline]
    fn add_token(&self) -> usize {
        // CORRECT: Release to publish the token
        self.tokens.fetch_add(1, Ordering::Release)
    }
}

#[repr(transparent)]
pub(crate) struct ThreadParker {
    ptr: *const ThreadInner,
}

impl ThreadParker {
    pub(crate) fn new() -> (Parker, Unparker) {
        let ptr = Box::into_raw(Box::new(ThreadInner::new()));
        if ptr.is_null() {
            panic!("Invalid allocation for ThreadParker");
        }
        let tp = Self { ptr };

        (Parker { inner: tp.clone() }, Unparker { inner: tp.clone() })
    }

    #[inline(always)]
    fn inner(&self) -> &ThreadInner {
        unsafe { &*self.ptr }
    }

    pub(crate) fn park(&self) {
        let inner = self.inner();

        // Fast path
        if inner.try_consume_token() {
            return;
        }

        // Slow path
        loop {
            // Try again to consume before blocking
            if inner.try_consume_token() {
                return;
            }

            // CRITICAL: futex_wait checks atomically
            // If tokens != PARKED when called, it returns immediately
            futex_wait(&inner.tokens, PARKED);

            // After wake (or spurious wakeup), recheck
            // Do not assume a token is available
        }
    }

    pub(crate) fn unpark(&self) {
        let inner = self.inner();

        // Always increment and always wake
        // This avoids the race where the wake could be lost
        inner.add_token();

        // Wake up ALL threads (safer to prevent deadlocks)
        // or at least one if you are sure about the logic
        futex_wake(&*inner.tokens);
    }
}

impl Clone for ThreadParker {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        ThreadParker { ptr: self.ptr }
    }
}

impl Drop for ThreadParker {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);

            let ptr = self.ptr as *mut ThreadInner;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

#[repr(transparent)]
pub(crate) struct Parker {
    inner: ThreadParker,
}

unsafe impl Send for Parker {}
unsafe impl Sync for Parker {}

impl Parker {
    pub(crate) fn park(&self) {
        self.inner.park()
    }
}

impl Clone for Parker {
    fn clone(&self) -> Self {
        Parker {
            inner: self.inner.clone(),
        }
    }
}

unsafe impl Send for Unparker {}
unsafe impl Sync for Unparker {}

#[repr(transparent)]
pub(crate) struct Unparker {
    inner: ThreadParker,
}

impl Unparker {
    pub(crate) fn unpark(&self) {
        self.inner.unpark()
    }
}

impl Clone for Unparker {
    fn clone(&self) -> Self {
        Unparker {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests_thread {
    use crate::core::thread::ThreadParker;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_unpark_before_park() {
        let (parker, unparker) = ThreadParker::new();
        unparker.unpark();
        parker.park();
    }

    #[test]
    fn test_unpark_after_park() {
        let t = thread::spawn(move || {
            let (parker, unparker) = ThreadParker::new();
            let mut ths = vec![];

            unparker.unpark();
            unparker.unpark();
            unparker.unpark();
            unparker.unpark();

            for i in 0..100 {
                let unparker = unparker.clone();
                let parker = parker.clone();
                ths.push(thread::spawn(move || {
                    parker.park();
                    thread::sleep(Duration::from_millis(1));
                    if i > 3 {
                        unparker.unpark();
                    }
                }));
            }

            for th in ths {
                th.join().unwrap();
            }
        });

        t.join().unwrap()
    }

    #[test]
    fn test_multiple_tokens() {
        let (parker, unparker) = ThreadParker::new();
        unparker.unpark();
        unparker.unpark();

        parker.park();
        parker.park();
    }
}
