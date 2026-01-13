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
        // First check if this is a dangling pointer (never allocated or default)
        let ptr = self.ptr;
        if is_dangling(ptr) {
            return None;
        }

        // SAFETY: We access only the atomic `strong` field via raw pointer.
        // The AnyRefInner struct itself is guaranteed to be allocated as long as
        // there's at least one weak reference (which we hold).
        // We do NOT create a reference to the entire AnyRefInner struct here,
        // only to the atomic field, which avoids conflicts with concurrent
        // drop_in_place of the data field in AnyRef::drop.
        let inner_ptr = ptr as *mut AnyRefInner;
        let strong_ptr = unsafe { ptr::addr_of!((*inner_ptr).strong) };
        let strong_atomic = unsafe { &*strong_ptr };

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
        //
        // Acquire ordering synchronizes with the Release in AnyRef::drop's fetch_sub,
        // ensuring we see all writes to the data before the strong count
        // was decremented to 0. If strong is 0, the CAS fails and we return None.
        if strong_atomic
            .fetch_update(Acquire, Relaxed, checked_increment)
            .is_ok()
        {
            // SAFETY: The strong count was > 0 and we've incremented it,
            // so the data is still alive.
            unsafe { Some(AnyRef::from_inner_in(self.get_mut_inner_ptr())) }
        } else {
            None
        }
    }

    /// Returns a reference to the inner `AnyRefInner` if the pointer is not dangling.
    ///
    /// # Safety Note
    /// This should only be used to access atomic fields (strong, weak counts).
    /// The data field may be in the process of being dropped if strong count is 0.
    #[inline]
    fn inner(&self) -> Option<&AnyRefInner> {
        let ptr = self.ptr;
        if is_dangling(ptr) {
            None
        } else {
            // SAFETY: If not dangling and we hold a weak ref, the AnyRefInner
            // struct itself is still allocated (only deallocated when weak count hits 0).
            // Callers must only access atomic fields (strong, weak).
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
        // Access only the atomic field via raw pointer to avoid data races
        let ptr = self.ptr;
        if is_dangling(ptr) {
            return 0;
        }

        let inner_ptr = ptr as *mut AnyRefInner;
        let strong_ptr = unsafe { ptr::addr_of!((*inner_ptr).strong) };
        unsafe { &*strong_ptr }.load(Relaxed)
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
        // Access only the atomic field via raw pointer to avoid data races
        let ptr = self.ptr;
        if is_dangling(ptr) {
            return 0;
        }

        let inner_ptr = ptr as *mut AnyRefInner;
        let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
        let count = unsafe { &*weak_ptr }.load(Relaxed);

        // Subtract 1 for the implicit weak reference held by strong refs
        if count > 0 { count - 1 } else { 0 }
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
        let ptr = self.ptr;

        // If dangling, just copy the dangling pointer
        if !is_dangling(ptr) {
            // Access only the atomic field via raw pointer
            let inner_ptr = ptr as *mut AnyRefInner;
            let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
            let weak_atomic = unsafe { &*weak_ptr };

            let old_size = weak_atomic.fetch_add(1, Relaxed);
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
        let ptr = self.ptr;

        // If dangling, nothing to do
        if is_dangling(ptr) {
            return;
        }

        // Access only the atomic field via raw pointer
        let inner_ptr = ptr as *mut AnyRefInner;
        let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
        let weak_atomic = unsafe { &*weak_ptr };

        if weak_atomic.fetch_sub(1, Release) == 1 {
            // We were the last weak reference, deallocate the AnyRefInner
            atomic::fence(Acquire);

            let layout = Layout::new::<AnyRefInner>();
            let ptr = self.ptr as *mut u8;

            unsafe {
                dealloc(ptr, layout);
            }
        }
    }
}
