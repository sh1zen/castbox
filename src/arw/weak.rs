use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::arw::Arw;
use crate::utils::is_dangling;
use std::alloc::{dealloc, Layout};
use std::num::NonZeroUsize;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::ptr;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

#[repr(transparent)]
pub struct WeakArw<T: Sized> {
    pub(crate) ptr: *const ArwInner<T>,
}

unsafe impl<T: Sized + Sync + Send> Send for WeakArw<T> {}
unsafe impl<T: Sized + Sync + Send> Sync for WeakArw<T> {}

impl<T> UnwindSafe for WeakArw<T> {}
impl<T> RefUnwindSafe for WeakArw<T> {}

impl<T> WeakArw<T> {
    #[inline]
    pub const fn new() -> WeakArw<T> {
        let ptr: *const ArwInner<T> = ptr::without_provenance(NonZeroUsize::MAX.get());
        WeakArw { ptr }
    }

    /// Attempts to upgrade the weak reference to a strong one.
    /// Returns `None` if the value has been dropped.
    pub fn upgrade(&self) -> Option<Arw<T>> {
        if is_dangling(self.ptr) {
            return None;
        }

        // SAFETY: Accediamo SOLO al campo `strong` usando ptr::addr_of!
        // senza creare un riferimento all'intero ArwInner.
        // Questo evita il conflitto di retag con drop_in_place su `val`.
        let strong_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).strong) };

        let mut cur = unsafe { (*strong_ptr).load(Relaxed) };

        loop {
            if cur == 0 {
                return None;
            }

            assert!(cur <= MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");

            // SAFETY: strong_ptr punta a un AtomicUsize valido
            match unsafe { (*strong_ptr).compare_exchange_weak(cur, cur + 1, Acquire, Relaxed) } {
                Ok(_) => {
                    return unsafe { Some(Arw::from_inner_in(self.ptr as *mut ArwInner<T>)) };
                }
                Err(old) => cur = old,
            }
        }
    }

    /// Returns the number of strong references.
    pub fn strong_count(&self) -> usize {
        if is_dangling(self.ptr) {
            return 0;
        }

        // SAFETY: Accesso solo al campo atomico via puntatore raw
        let strong_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).strong) };
        unsafe { (*strong_ptr).load(Relaxed) }
    }

    /// Returns the number of weak references.
    pub fn weak_count(&self) -> usize {
        if is_dangling(self.ptr) {
            return 0;
        }

        // SAFETY: Accesso solo al campo atomico via puntatore raw
        let weak_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).weak) };
        unsafe { (*weak_ptr).load(Relaxed) }
    }
}

impl<T> Clone for WeakArw<T> {
    fn clone(&self) -> WeakArw<T> {
        if !is_dangling(self.ptr) {
            // SAFETY: Accesso solo al campo atomico via puntatore raw
            let weak_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).weak) };

            let old_size = unsafe { (*weak_ptr).fetch_add(1, Relaxed) };
            if old_size > MAX_REFCOUNT {
                abort();
            }
        }

        Self { ptr: self.ptr }
    }
}

impl<T> Default for WeakArw<T> {
    fn default() -> WeakArw<T> {
        WeakArw::new()
    }
}

impl<T> PtrInterface<T> for WeakArw<T> {
    #[inline]
    fn get_mut_inner_ptr(&self) -> *mut ArwInner<T> {
        self.ptr as *mut ArwInner<T>
    }

    #[inline]
    unsafe fn from_inner_in(ptr: *mut ArwInner<T>) -> Self {
        assert!(!ptr.is_null());
        Self { ptr }
    }
}

impl<T> Drop for WeakArw<T> {
    fn drop(&mut self) {
        if is_dangling(self.ptr) {
            return;
        }

        // SAFETY: Accesso solo al campo atomico via puntatore raw
        let weak_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).weak) };

        if unsafe { (*weak_ptr).fetch_sub(1, Release) } == 1 {
            atomic::fence(Acquire);

            // A questo punto weak == 0, siamo gli unici e possiamo deallocare
            let layout = Layout::new::<ArwInner<T>>();
            unsafe {
                dealloc(self.ptr as *mut u8, layout);
            }
        }
    }
}
