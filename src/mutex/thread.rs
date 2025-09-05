use std::sync::atomic;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::thread::Thread;
use std::{hint, thread};

struct ThreadInner {
    tokens: AtomicIsize,
    ref_count: AtomicUsize,
    state: AtomicUsize,
    thread: Thread,
}

#[repr(transparent)]
pub(crate) struct ThreadParker {
    ptr: *const ThreadInner,
}

unsafe impl Send for ThreadParker {}
unsafe impl Sync for ThreadParker {}

const RUNNING: usize = 0;
const PARKING: usize = 1;
const PARKED: usize = 2;

impl ThreadParker {
    pub(crate) fn new() -> Self {
        let ptr = Box::into_raw(Box::new(ThreadInner {
            tokens: AtomicIsize::new(1),
            ref_count: AtomicUsize::new(1),
            state: AtomicUsize::new(RUNNING),
            thread: thread::current(),
        }));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mutex");
        }
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &ThreadInner {
        unsafe { &*self.ptr }
    }

    /// Consuma un token se disponibile, altrimenti blocca.
    pub(crate) fn park(&self) {
        let inner = self.inner();

        if inner.tokens.fetch_sub(1, Ordering::AcqRel) > 1 {
            return;
        }

        // set to parking
        loop {
            if inner
                .state
                .compare_exchange(RUNNING, PARKING, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }

            // transizione PARKING -> PARKED (se fallisce, qualcuno ci ha svegliato)
            if inner
                .state
                .compare_exchange(PARKING, PARKED, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // Siamo ufficialmente parcheggiati: chiamiamo park() senza tenere nessun lock
                if inner.tokens.load(Ordering::Acquire) == 0
                    && thread::current().id() == inner.thread.id()
                {
                    thread::park();
                }

                // al wake: dobbiamo pulire lo stato
                // proviamo a fare PARKED -> EMPTY; se fallisce, probabilmente l'unparker ha già fatto il lavoro.
                inner.state.store(RUNNING, Ordering::Release);
                break;
            }
        }
    }

    /// Aggiunge un token e sveglia il thread se è parcheggiato.
    pub(crate) fn unpark(&self) {
        let inner = self.inner();

        inner.tokens.fetch_add(1, Ordering::Release);

        loop {
            match inner.state.load(Ordering::Acquire) {
                PARKED => {
                    inner.thread.unpark();
                    Self::spin_loop(20);
                    break;
                }
                RUNNING => {
                    break;
                }
                _ => {
                    Self::spin_loop(10);
                }
            }
        }
    }

    #[inline(always)]
    fn spin_loop(mut n: usize) {
        while n > 0 {
            hint::spin_loop();
            n -= 1;
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
        if self.inner().ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            atomic::fence(Ordering::Acquire);

            let ptr = self.ptr as *mut ThreadInner;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

#[cfg(test)]
mod tests_thread {
    use super::*;

    #[test]
    fn test_unpark_before_park() {
        let parker = ThreadParker::new();
        parker.unpark();
        parker.park();
    }

    #[test]
    fn test_unpark_after_park() {
        let t = thread::spawn(move || {
            let parker = ThreadParker::new();

            let pc = parker.clone();
            let t = thread::spawn(move || {
                for _ in 0..120 {
                    pc.unpark();
                }
            });

            for _ in 0..100 {
                parker.park();
            }

            t.join().unwrap();

            123
        });

        assert_eq!(t.join().unwrap(), 123);
    }

    #[test]
    fn test_multiple_tokens() {
        let parker = ThreadParker::new();
        parker.unpark();
        parker.unpark();

        parker.park();
        parker.park();
    }
}
