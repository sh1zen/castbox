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
    /// Constructs a new `WeakARW`, without allocating any memory.
    /// Calling [`upgrade`] on the return value always gives [`None`].
    ///
    /// ```
    /// use castbox::WeakArw;
    /// let empty: WeakArw<String> = WeakArw::new();
    /// assert!(empty.upgrade().is_none());
    /// ```
    #[inline]
    pub const fn new() -> WeakArw<T> {
        let ptr: *const ArwInner<T> = ptr::without_provenance(NonZeroUsize::MAX.get());
        let ptr = ptr as *mut ArwInner<T>;
        // SAFETY: we know `addr` is non-zero.
        WeakArw { ptr }
    }

    /// Attempts to upgrade the weak reference to a strong one.
    /// Returns `None` if the value has been dropped.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(33);
    /// let w = a.downgrade();
    /// drop(a);
    /// assert!(w.upgrade().is_none());
    /// ```
    pub fn upgrade(&self) -> Option<Arw<T>> {
        #[inline]
        fn checked_increment(n: usize) -> Option<usize> {
            if n == 0 {
                return None;
            }
            assert!(n <= MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");
            Some(n + 1)
        }

        // We use a CAS loop to increment the strong count instead of a
        // fetch_add as this function should never take the reference count
        // from zero to one.
        if self
            .inner()?
            .strong
            .fetch_update(Acquire, Relaxed, checked_increment)
            .is_ok()
        {
            // SAFETY: pointer is not null, verified in checked_increment
            unsafe { Some(Arw::from_inner_in(self.get_mut_inner_ptr())) }
        } else {
            None
        }
    }

    #[inline]
    fn inner(&self) -> Option<&ArwInner<T>> {
        let ptr = self.ptr;
        if is_dangling(ptr) {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }

    /// Returns the number of strong references.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(1);
    /// let w = a.downgrade();
    /// assert_eq!(w.strong_count(), 1);
    /// ```
    pub fn strong_count(&self) -> usize {
        if let Some(inner) = self.inner() {
            inner.strong.load(Relaxed)
        } else {
            0
        }
    }

    /// Returns the number of weak references.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(1);
    /// let w1 = a.downgrade();
    /// let w2 = w1.clone();
    /// assert_eq!(w2.weak_count(), 3); // includes implicit
    /// ```
    pub fn weak_count(&self) -> usize {
        if let Some(inner) = self.inner() {
            inner.weak.load(Relaxed)
        } else {
            0
        }
    }
}

impl<T> Clone for WeakArw<T> {
    /// Clones the weak reference, incrementing the weak count.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(8);
    /// let w1 = a.downgrade();
    /// let w2 = w1.clone();
    /// ```
    fn clone(&self) -> WeakArw<T> {
        if let Some(inner) = self.inner() {
            let old_size = inner.weak.fetch_add(1, Relaxed);

            if old_size > MAX_REFCOUNT {
                abort();
            }
        }

        Self { ptr: self.ptr }
    }
}

impl<T> Default for WeakArw<T> {
    /// Constructs a new `Weak<T>`, without allocating memory.
    /// Calling [`upgrade`] on the return value always
    /// gives [`None`].
    ///
    /// ```
    /// use castbox::WeakArw;
    ///
    /// let empty: WeakArw<String> = Default::default();
    /// assert!(empty.upgrade().is_none());
    /// ```
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
        debug_assert!(!ptr.is_null());
        Self { ptr }
    }
}

impl<T> Drop for WeakArw<T> {
    fn drop(&mut self) {
        let inner = if let Some(inner) = self.inner() {
            inner
        } else {
            return;
        };
        if inner.weak.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);

            let layout = Layout::new::<ArwInner<T>>();
            let ptr = self.ptr as *mut u8;

            unsafe {
                dealloc(ptr, layout);
            }
        }
    }
}
