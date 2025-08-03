use crate::anyref::inner::{AnyRefInner, MAX_REFCOUNT};
use crate::anyref::ptr_interface::PtrInterface;
use crate::anyref::strong::AnyRef;
use crate::utils::is_dangling;
use std::alloc::{dealloc, Layout};
use std::num::NonZeroUsize;
use std::process::abort;
use std::ptr;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

#[repr(transparent)]
pub struct WeakAnyRef {
    pub(crate) ptr: *const AnyRefInner,
}

unsafe impl Send for WeakAnyRef {}
unsafe impl Sync for WeakAnyRef {}

impl WeakAnyRef {
    /// Constructs a new `WeakAnyRef`, without allocating any memory.
    /// Calling [`upgrade`] on the return value always gives [`None`].
    ///
    /// ```
    /// use castbox::WeakAnyRef;
    /// let empty: WeakAnyRef = WeakAnyRef::new();
    /// assert!(empty.upgrade().is_none());
    /// ```
    #[inline]
    pub const fn new() -> WeakAnyRef {
        let ptr: *const AnyRefInner = ptr::without_provenance(NonZeroUsize::MAX.get());
        let ptr = ptr as *mut AnyRefInner;
        // SAFETY: we know `addr` is non-zero.
        WeakAnyRef { ptr }
    }

    /// Attempts to upgrade the weak reference to a strong one.
    /// Returns `None` if the value has been dropped.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(33);
    /// let w = a.downgrade();
    /// drop(a);
    /// assert!(w.upgrade().is_none());
    /// ```
    pub fn upgrade(&self) -> Option<AnyRef> {
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
            unsafe { Some(AnyRef::from_inner_in(self.get_mut_inner_ptr())) }
        } else {
            None
        }
    }

    #[inline]
    fn inner(&self) -> Option<&AnyRefInner> {
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
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(1);
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
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(1);
    /// let w1 = a.downgrade();
    /// let w2 = w1.clone();
    /// assert_eq!(w2.weak_count(), 2); // includes implicit
    /// ```
    pub fn weak_count(&self) -> usize {
        if let Some(inner) = self.inner() {
            inner.weak.load(Relaxed) - 1
        } else {
            0
        }
    }
}

impl Clone for WeakAnyRef {
    /// Clones the weak reference, incrementing the weak count.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(8);
    /// let w1 = a.downgrade();
    /// let w2 = w1.clone();
    /// ```
    fn clone(&self) -> WeakAnyRef {
        if let Some(inner) = self.inner() {
            let old_size = inner.weak.fetch_add(1, Relaxed);

            if old_size > MAX_REFCOUNT {
                abort();
            }
        }

        Self { ptr: self.ptr }
    }
}

impl Default for WeakAnyRef {
    /// Constructs a new `Weak<T>`, without allocating memory.
    /// Calling [`upgrade`] on the return value always
    /// gives [`None`].
    ///
    /// ```
    /// use castbox::WeakAnyRef;
    ///
    /// let empty: WeakAnyRef = Default::default();
    /// assert!(empty.upgrade().is_none());
    /// ```
    fn default() -> WeakAnyRef {
        WeakAnyRef::new()
    }
}

impl PtrInterface for WeakAnyRef {
    #[inline]
    fn get_mut_inner_ptr(&self) -> *mut AnyRefInner {
        self.ptr as *mut AnyRefInner
    }

    #[inline]
    unsafe fn from_inner_in(ptr: *mut AnyRefInner) -> Self {
        debug_assert!(!ptr.is_null());
        Self { ptr }
    }
}

impl Drop for WeakAnyRef {
    fn drop(&mut self) {
        let inner = if let Some(inner) = self.inner() {
            inner
        } else {
            return;
        };
        if inner.weak.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);

            let layout = Layout::new::<AnyRefInner>();
            let ptr = self.ptr as *mut u8;

            unsafe {
                dealloc(ptr, layout);
            }
        }
    }
}
