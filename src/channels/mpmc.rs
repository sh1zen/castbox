use crate::collections::AtomicVec;
use crate::mutex::{Mutex, MutexType};
use std::sync::atomic;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct MpmcInner<T> {
    buffer: AtomicVec<T>,
    notify: Mutex,
    /// cloned ref
    ref_count: AtomicUsize,

    bounded: usize,

    closed: AtomicBool,
}

pub struct Mpmc<T> {
    ptr: *const MpmcInner<T>,
}

unsafe impl<T: Send> Send for Mpmc<T> {}
unsafe impl<T: Sync> Sync for Mpmc<T> {}

impl<T> Mpmc<T> {
    /// Crea un canale MPMC condiviso mediante Arc.
    pub fn new() -> Mpmc<T> {
        let ptr = Box::into_raw(Box::new(MpmcInner {
            buffer: AtomicVec::new(),
            notify: Mutex::new(),
            ref_count: AtomicUsize::new(1),
            bounded: 0,
            closed: AtomicBool::new(false),
        }));

        if ptr.is_null() {
            panic!("Happened an invalid allocation for Mpmc");
        }

        Self { ptr }
    }

    pub fn bounded(n: usize) -> Mpmc<T> {
        let c = Self::new();
        c.inner_mut().bounded = n;
        c
    }

    #[inline(always)]
    fn inner(&self) -> &MpmcInner<T> {
        unsafe { &*self.ptr }
    }

    #[inline(always)]
    fn inner_mut(&self) -> &mut MpmcInner<T> {
        let ptr = self.ptr as *mut MpmcInner<T>;
        unsafe { &mut *ptr }
    }

    /// Invio non bloccante. Ritorna Err(value) se il canale è chiuso.
    pub fn send(&self, value: T) -> Result<(), T> {
        let inner = self.inner();

        if inner.closed.load(Ordering::Acquire) {
            return Err(value);
        }

        if inner.bounded > 0 && inner.buffer.len() >= inner.bounded{
            return Err(value)
        }

        inner.buffer.push(value);

        let _ = inner.notify.wake(MutexType::Group);

        Ok(())
    }

    /// Prova a ricevere senza bloccare.
    pub fn try_recv(&self) -> Option<T> {
        self.inner().buffer.pop()
    }

    /// Riceve bloccando finché non arriva un elemento o il canale è chiuso e vuoto.
    pub fn recv(&self) -> Option<T> {
        loop {
            if let Some(msg) = self.inner().buffer.pop() {
                return Some(msg);
            }

            if self.inner().closed.load(Ordering::Acquire) {
                return None;
            }

            if !self.inner().notify.suspend(MutexType::Group) {
                std::hint::spin_loop();
            }
        }
    }

    /// Chiude il canale: nuovi send falliranno; sveglia tutti i consumer parcheggiati.
    pub fn close(&self) {
        self.inner().closed.store(true, Ordering::Release);
        // sveglia tutti i consumer affinché possano terminare
        self.inner().notify.wake_all(MutexType::Group);
    }
}

impl<T> Clone for Mpmc<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for Mpmc<T> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);

            let ptr = self.ptr as *mut MpmcInner<T>;

            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

