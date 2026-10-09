use crate::arw::Arw;
use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::utils::is_dangling;
use std::alloc::{Layout, dealloc};
use std::num::NonZeroUsize;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::ptr;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// A non-owning handle to an [`Arw<T>`] allocation.
///
/// A weak handle retains reference-counting storage without keeping the value
/// alive. [`Self::upgrade`] creates a strong handle while the value is alive,
/// or returns `None` after the last strong handle has been dropped. `WeakArw<T>`
/// implements `Send` and `Sync` when `T` implements both traits.
#[repr(transparent)]
pub struct WeakArw<T: Sized> {
    pub(crate) ptr: *const ArwInner<T>,
}

pub(crate) struct ImplicitWeak<T> {
    pub(crate) ptr: *const ArwInner<T>,
}

impl<T> Drop for ImplicitWeak<T> {
    fn drop(&mut self) {
        unsafe { release_weak(self.ptr) };
    }
}

unsafe fn release_weak<T>(ptr: *const ArwInner<T>) {
    let weak = unsafe { &*ptr::addr_of!((*ptr).weak) };
    if weak.fetch_sub(1, Release) == 1 {
        atomic::fence(Acquire);
        unsafe { dealloc(ptr.cast_mut().cast(), Layout::new::<ArwInner<T>>()) };
    }
}

unsafe impl<T: Sized + Sync + Send> Send for WeakArw<T> {}
unsafe impl<T: Sized + Sync + Send> Sync for WeakArw<T> {}

impl<T> UnwindSafe for WeakArw<T> {}
impl<T> RefUnwindSafe for WeakArw<T> {}

impl<T> WeakArw<T> {
    /// Creates an empty weak handle without allocating memory.
    ///
    /// [`Self::upgrade`] always returns `None` for an empty handle.
    ///
    /// # Examples
    /// ```
    /// use castbox::WeakArw;
    /// let weak = WeakArw::<String>::new();
    /// assert!(weak.upgrade().is_none());
    /// assert_eq!(weak.strong_count(), 0);
    /// assert_eq!(weak.weak_count(), 0);
    /// ```
    #[inline]
    pub const fn new() -> WeakArw<T> {
        let ptr: *const ArwInner<T> = ptr::without_provenance(NonZeroUsize::MAX.get());
        WeakArw { ptr }
    }

    /// Attempts to upgrade the weak reference to a strong one.
    /// Returns `None` if the value has been dropped.
    /// A successful upgrade increments the strong count and keeps the value alive.
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
    /// Empty handles report zero. The count can change immediately on another thread.
    pub fn strong_count(&self) -> usize {
        if is_dangling(self.ptr) {
            return 0;
        }

        // SAFETY: Accesso solo al campo atomico via puntatore raw
        let strong_ptr: *const AtomicUsize = unsafe { ptr::addr_of!((*self.ptr).strong) };
        unsafe { (*strong_ptr).load(Relaxed) }
    }

    /// Returns the number of explicit weak references, excluding the implicit one.
    /// This is an estimate when other threads are cloning or dropping references.
    /// Empty handles report zero; allocated handles include themselves in the count.
    pub fn weak_count(&self) -> usize {
        if is_dangling(self.ptr) {
            return 0;
        }

        // SAFETY: Accesso solo al campo atomico via puntatore raw
        let count_ptr = unsafe { ptr::addr_of!((*self.ptr).explicit_weak) };
        unsafe { (*count_ptr).load(Relaxed) }
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
            let count_ptr = unsafe { ptr::addr_of!((*self.ptr).explicit_weak) };
            unsafe { (*count_ptr).fetch_add(1, Relaxed) };
        }

        Self { ptr: self.ptr }
    }
}

impl<T> Default for WeakArw<T> {
    /// Creates an empty weak handle, equivalent to [`WeakArw::new`].
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

        let count_ptr = unsafe { ptr::addr_of!((*self.ptr).explicit_weak) };
        unsafe { (*count_ptr).fetch_sub(1, Relaxed) };
        unsafe { release_weak(self.ptr) };
    }
}
