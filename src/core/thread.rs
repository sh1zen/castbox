use crate::core::futex::{futex_wait, futex_wake};
use std::sync::atomic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, Thread};

type State = usize;

const PARKED: State = 0;
const UNPARKED: State = 1;

/// Parker inner state; shared via Arc to avoid use-after-free.
struct ThreadInner {
    /// Number of available tokens (permits). Do not use negative values as persistent state.
    tokens: AtomicUsize,
    /// n.ro of current clones
    ref_count: AtomicUsize,
    /// Handle of the thread associated with this parker.
    thread: Thread,
}

impl ThreadInner {
    fn new() -> Self {
        Self {
            tokens: AtomicUsize::new(UNPARKED),
            ref_count: AtomicUsize::new(1),
            thread: thread::current(),
        }
    }

    /// Atomic attempt to consume a token; returns true on success.
    /// Implemented with compare_exchange to avoid taking the counter negative
    /// under concurrency.
    fn try_consume_token(&self) -> bool {
        self.tokens
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| {
                if v > PARKED { Some(v - 1) } else { None }
            })
            .unwrap_or(PARKED)
            >= UNPARKED
    }

    /// Increment tokens (unpark).
    fn add_token(&self) -> usize {
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

    /// Park: consume a token if available, otherwise wait for a notification.
    /// The loop handles spurious wakeups and persistent notifications.
    pub(crate) fn park(&self) {
        let inner = self.inner();

        // Sleep path: double-check before and after sleeping to avoid races
        loop {
            // Re-check before sleeping to avoid missed wakeups.
            if inner.try_consume_token() {
                return;
            }

            // Wait until the value is still PARKED. If it changes, wait returns immediately.
            futex_wait(&inner.tokens, PARKED);

            // After waking (or spuriously), attempt to consume a token.
            if inner.try_consume_token() {
                return;
            }
            // Otherwise, we observed a spurious wake or lost the race; continue waiting.
        }
    }

    /// Unpark: add a token and notify the parker. Never blocks.
    pub(crate) fn unpark(&self) {
        let inner = self.inner();
        // Add token (publish) and wake only if there were no available tokens.
        if inner.add_token() == PARKED {
            // Ensure release-acquire ordering with park().
            futex_wake(&inner.tokens);
        }
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
    use super::*;
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
