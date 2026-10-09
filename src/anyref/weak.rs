use crate::anyref::inner::{AnyRefInner, MAX_REFCOUNT};
use crate::anyref::ptr_interface::PtrInterface;
use crate::anyref::strong::AnyRef;
use crate::utils::is_dangling;
use std::alloc::{Layout, dealloc};
use std::num::NonZeroUsize;
use std::process::abort;
use std::ptr;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// A non-owning handle to an [`AnyRef`] allocation.
///
/// A weak handle retains reference-counting storage without keeping the value
/// alive. [`Self::upgrade`] creates a strong handle while the value is alive,
/// or returns `None` after the last strong handle has been dropped. Empty handles
/// created with [`Self::new`] or `Default::default()` never upgrade.
#[repr(transparent)]
pub struct WeakAnyRef {
    pub(crate) ptr: *const AnyRefInner,
}

// Keeps the allocation alive during payload destruction without contributing
// to the public count of explicit weak references.
pub(crate) struct ImplicitWeak {
    pub(crate) ptr: *const AnyRefInner,
}

impl Drop for ImplicitWeak {
    fn drop(&mut self) {
        unsafe { release_weak(self.ptr) };
    }
}

unsafe fn release_weak(ptr: *const AnyRefInner) {
    let weak = unsafe { &*ptr::addr_of!((*ptr).weak) };
    if weak.fetch_sub(1, Release) == 1 {
        atomic::fence(Acquire);
        unsafe { dealloc(ptr.cast_mut().cast(), Layout::new::<AnyRefInner>()) };
    }
}

// SAFETY: Upgrading can only recover payloads inserted with Send + Sync bounds.
// Weak handles otherwise access only the atomic allocation counters.
unsafe impl Send for WeakAnyRef {}
unsafe impl Sync for WeakAnyRef {}

impl WeakAnyRef {
    /// Constructs a new `WeakAnyRef`, without allocating any memory.
    /// Calling [`Self::upgrade`] on the return value always gives [`None`].
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
    /// A successful upgrade increments the strong count and keeps the value alive.
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

        let mut count = strong_atomic.load(Relaxed);
        loop {
            if count == 0 {
                return None;
            }
            assert!(count < MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");
            match strong_atomic.compare_exchange_weak(count, count + 1, Acquire, Relaxed) {
                Ok(_) => return unsafe { Some(AnyRef::from_inner_in(self.get_mut_inner_ptr())) },
                Err(current) => count = current,
            }
        }
    }

    /// Returns the number of strong references.
    /// Empty handles report zero. The count can change immediately on another thread.
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

    /// Returns the number of explicit weak references, excluding the implicit one.
    /// This is an estimate when other threads are cloning or dropping references.
    /// Empty handles report zero; allocated handles include themselves in the count.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(1);
    /// let w1 = a.downgrade();
    /// let w2 = w1.clone();
    /// assert_eq!(w2.weak_count(), 2);
    /// ```
    pub fn weak_count(&self) -> usize {
        // Access only the atomic field via raw pointer to avoid data races
        let ptr = self.ptr;
        if is_dangling(ptr) {
            return 0;
        }

        let inner_ptr = ptr as *mut AnyRefInner;
        let count_ptr = unsafe { ptr::addr_of!((*inner_ptr).explicit_weak) };
        unsafe { &*count_ptr }.load(Relaxed)
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
            let count_ptr = unsafe { ptr::addr_of!((*inner_ptr).explicit_weak) };
            unsafe { &*count_ptr }.fetch_add(1, Relaxed);
        }

        Self { ptr: self.ptr }
    }
}

impl Default for WeakAnyRef {
    /// Constructs an empty `WeakAnyRef`, without allocating memory.
    /// Calling [`WeakAnyRef::upgrade`] on the return value always
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

        let count_ptr = unsafe { ptr::addr_of!((*ptr).explicit_weak) };
        unsafe { &*count_ptr }.fetch_sub(1, Relaxed);
        unsafe { release_weak(ptr) };
    }
}
