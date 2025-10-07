use crate::core::scondvar::SCondVar;
use crate::core::smutex::SMutex;
use crate::mutex::Mutex;
use std::sync::atomic;
use std::sync::atomic::{AtomicUsize, Ordering};

struct BarrierInner {
    ref_count: AtomicUsize,
    waiters: AtomicUsize,
    bucket: usize,
    mutex: SMutex,
    cond: SCondVar,
}

#[repr(transparent)]
pub struct Barrier {
    ptr: *const BarrierInner,
}

unsafe impl Send for Barrier {}
unsafe impl Sync for Barrier {}

impl Barrier {
    pub fn new() -> Barrier {
        Self::init(1, 0)
    }

    /// Quando `n` waiters sono raggiunti, resettiamo a `bucket`.
    /// Se `bucket == 0` → disabilitiamo i prossimi gruppi.
    pub fn with_capacity(n: usize, bucket: usize) -> Barrier {
        Self::init(n + 2, if bucket == 0 { 0 } else { bucket + 2 })
    }

    fn init(n: usize, bucket: usize) -> Barrier {
        let ptr = Box::into_raw(Box::new(BarrierInner {
            ref_count: AtomicUsize::new(1),
            waiters: AtomicUsize::new(n),
            bucket,
            mutex: SMutex::new(),
            cond: SCondVar::new(),
        }));

        if ptr.is_null() {
            panic!("Invalid allocation for Barrier");
        }
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &BarrierInner {
        unsafe { &*self.ptr }
    }

    pub fn count(&self) -> usize {
        self.inner().waiters.load(Ordering::Acquire)
    }

    pub fn wait(&self) {
        let inner = self.inner();

        // Leggiamo il valore attuale
        let waiters = inner.waiters.load(Ordering::Acquire);

        // Caso base: barriera "spenta"
        if waiters == 0 || inner.ref_count.load(Ordering::Acquire) == 1 {
            return;
        }

        let guard = inner.mutex.lock();

        if waiters > 1 {
            // Decrementiamo
            let new_val = inner.waiters.fetch_sub(1, Ordering::AcqRel) - 1;

            if new_val == 2 {
                // Penultimo: risveglia tutti e resetta la barriera
                inner.waiters.store(inner.bucket, Ordering::Release);
                self.release();
            } else {
                // Deve aspettare la notifica
                let _ = inner.cond.wait(guard);
            }
        } else {
            // Caso particolare: ultimo → deve attendere la notifica
            let _ = inner.cond.wait(guard);
        }
    }

    #[inline(always)]
    pub fn release(&self) {
        self.inner().cond.notify_all();
    }
}

impl Clone for Barrier {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Release);
        Barrier { ptr: self.ptr }
    }
}

impl Drop for Barrier {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            let ptr = self.ptr as *mut BarrierInner;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl Default for Barrier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests_barrier {
    use super::Barrier;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn single_thread_wait() {
        let barrier = Barrier::new();
        // Con un solo thread la wait deve ritornare subito
        barrier.wait();
        assert_eq!(barrier.count(), 1); // la count non cambia
    }

    #[test]
    fn multi_wait() {
        let barrier = Barrier::new();

        let mut handles = vec![];
        for _ in 0..10 {
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                b.wait();
            }));
        }

        thread::sleep(Duration::from_millis(100));
        barrier.release();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn multiple_threads_wait() {
        let barrier = Barrier::with_capacity(3, 0);
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..3 {
            let b = barrier.clone();
            let c = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                c.fetch_add(1, Ordering::SeqCst);
                b.wait(); // tutti i thread devono attendere qui
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Tutti i thread hanno incrementato due volte il counter
        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn barrier_reuse_with_bucket() {
        let barrier = Barrier::with_capacity(2, 1);
        let counter = Arc::new(AtomicUsize::new(0));

        let b1 = barrier.clone();
        let c1 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            c1.fetch_add(1, Ordering::SeqCst);
            b1.wait();
            c1.fetch_add(1, Ordering::SeqCst);
        });

        let b2 = barrier.clone();
        let c2 = Arc::clone(&counter);
        let t2 = thread::spawn(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            b2.wait();
            c2.fetch_add(1, Ordering::SeqCst);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Dopo il primo gruppo, la bucket permette a un altro thread di passare subito
        let b3 = barrier.clone();
        let c3 = Arc::clone(&counter);
        let t3 = thread::spawn(move || {
            c3.fetch_add(1, Ordering::SeqCst);
            b3.wait();
            c3.fetch_add(1, Ordering::SeqCst);
        });

        t3.join().unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn concurrent_threads_with_delays() {
        let barrier = Arc::new(Barrier::with_capacity(3, 0));
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for i in 0..3 {
            let b = Arc::clone(&barrier);
            let c = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                thread::sleep(Duration::from_millis(i * 10));
                c.fetch_add(1, Ordering::SeqCst);
                b.wait();
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }
}
