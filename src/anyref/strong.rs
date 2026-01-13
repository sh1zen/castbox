use crate::anyref::inner::{AnyRefInner, MAX_REFCOUNT};
use crate::anyref::ptr_interface::PtrInterface;
use crate::anyref::weak::WeakAnyRef;
use crate::utils::is_dangling;
use crossync::sync::{WatchGuardMut, WatchGuardRef};
use std::any::{Any, TypeId};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::{fmt, hint, ptr};

#[repr(transparent)]
pub struct AnyRef {
    ptr: *const AnyRefInner,
}

unsafe impl Send for AnyRef {}
unsafe impl Sync for AnyRef {}

impl UnwindSafe for AnyRef {}
impl RefUnwindSafe for AnyRef {}

impl AnyRef {
    /// Creates a new `AnyRef` containing the given value.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(42);
    /// assert_eq!(a.as_ref::<i32>(), 42);
    /// ```
    pub fn new<T>(value: T) -> Self
    where
        T: Any + Sized,
    {
        unsafe { Self::from_inner(Box::leak(Box::new(AnyRefInner::new(value)))) }
    }

    /// Attempts to extract the inner value if there is exactly one strong reference.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(123i32);
    /// let value = AnyRef::try_unwrap::<i32>(a).unwrap();
    /// assert_eq!(value, 123i32);
    /// ```
    pub fn try_unwrap<T>(this: Self) -> Result<T, Self>
    where
        T: Any + Sized,
    {
        let inner_ptr = this.ptr as *mut AnyRefInner;

        // SAFETY: We access only atomic fields via raw pointer arithmetic
        let strong_ptr = unsafe { ptr::addr_of!((*inner_ptr).strong) };
        let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
        let type_id = unsafe { ptr::addr_of!((*inner_ptr).type_id).read() };

        // Attempt to become sole owner by changing strong from 1 -> 0
        if unsafe { &*strong_ptr }
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_err()
        {
            return Err(this);
        }

        // Synchronize with the decrement that triggered deletion semantics
        atomic::fence(Acquire);

        // Check weak count. If it's not the implicit weak (1), restore and return Err.
        let weak_count = unsafe { &*weak_ptr }.load(Relaxed);
        if weak_count != 1 {
            // Restore strong to 1 so the object remains live and return the original AnyRef.
            unsafe { &*strong_ptr }.store(1, Release);
            return Err(this);
        }

        // We will consume `this` on success; keep it alive for now
        let this = ManuallyDrop::new(this);

        // If the type doesn't match, restore strong count and return Err
        if type_id != TypeId::of::<T>() {
            // Restore strong count back to 1 so the AnyRef remains valid
            unsafe { &*strong_ptr }.store(1, Release);
            return Err(ManuallyDrop::into_inner(this));
        }

        // SAFETY: We've verified the type and have exclusive access.
        // We need to extract the value without triggering a double-drop.
        let data_ptr = unsafe { ptr::addr_of_mut!((*inner_ptr).data) };

        // Extract the value from the Box<dyn Any>
        let elem: T = unsafe {
            let mut guard = (*data_ptr).lock_exclusive();

            // Take the Box<dyn Any> out of the guard, replacing with a dummy
            let boxed_any: Box<dyn Any> = std::mem::replace(&mut *guard, Box::new(()));
            drop(guard);

            // Downcast to Box<T> and unbox
            match boxed_any.downcast::<T>() {
                Ok(boxed_t) => *boxed_t,
                Err(_) => unreachable!("Type mismatch after type_id check"),
            }
        };

        // Make a weak pointer to clean up the implicit strong-weak reference
        // and deallocate the AnyRefInner when it drops
        let _weak = WeakAnyRef { ptr: this.ptr };

        // Drop the data field - it now contains a dummy Box<()> which is safe to drop
        unsafe {
            let data_field_ptr = ptr::addr_of_mut!((*inner_ptr).data);
            ptr::drop_in_place(data_field_ptr);
        }

        Ok(elem)
    }

    /// Returns a reference to the inner `AnyRefInner`.
    ///
    /// # Safety
    /// This is safe to call as long as there is at least one strong reference,
    /// which is guaranteed by holding an `AnyRef`.
    #[inline]
    pub(crate) fn inner(&self) -> &AnyRefInner {
        // SAFETY: While this AnyRef is alive we're guaranteed
        // that the inner pointer is valid and the data hasn't been dropped.
        unsafe { &*self.ptr }
    }

    #[inline]
    fn inner_mut(&mut self) -> &mut AnyRefInner {
        let ptr = self.get_mut_inner_ptr();
        unsafe { &mut *ptr }
    }

    #[inline]
    pub fn map<T, U: 'static, F>(self, func: F) -> AnyRef
    where
        T: Any,
        F: FnOnce(WatchGuardRef<'_, T>) -> U,
    {
        let ptr = self.as_ref::<T>();
        AnyRef::new(func(ptr))
    }

    /// Returns a raw pointer to the contained type, if possible.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(50);
    /// let ptr = unsafe { a.as_cast_ptr::<i32>() };
    /// unsafe { assert_eq!(*ptr, 50); }
    /// ```
    pub unsafe fn as_cast_ptr<T: Any>(&self) -> *const T {
        if self.inner().type_id != TypeId::of::<T>() {
            panic!(
                "AnyRef: wrong cast in as_ref::<{}>()",
                std::any::type_name::<T>()
            );
        }
        let ptr = self.as_ptr();
        ptr as *const T
    }

    /// Returns `true` if the `AnyRef` is the only strong reference to the value.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new("unique");
    /// assert!(AnyRef::is_unique(&a));
    /// let b = a.clone();
    /// assert!(!AnyRef::is_unique(&a));
    /// ```
    pub fn is_unique(this: &Self) -> bool {
        let inner_ptr = this.ptr as *mut AnyRefInner;

        // SAFETY: Access atomic fields via raw pointer to avoid creating reference
        let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
        let strong_ptr = unsafe { ptr::addr_of!((*inner_ptr).strong) };

        // Lock the weak pointer count if we appear to be the sole weak pointer holder.
        if unsafe { &*weak_ptr }
            .compare_exchange(1, usize::MAX, Acquire, Relaxed)
            .is_ok()
        {
            let unique = unsafe { &*strong_ptr }.load(Acquire) == 1;
            unsafe { &*weak_ptr }.store(1, Release);
            unique
        } else {
            false
        }
    }

    /// Convert into a weak reference
    /// # Example
    ///
    /// ```
    /// use castbox::AnyRef;
    /// let five = AnyRef::new(5);
    /// let weak_five = AnyRef::downgrade(&five);
    /// ```
    pub fn downgrade(&self) -> WeakAnyRef {
        let inner_ptr = self.ptr as *mut AnyRefInner;

        // SAFETY: Access atomic field via raw pointer
        let weak_ptr = unsafe { ptr::addr_of!((*inner_ptr).weak) };
        let weak_atomic = unsafe { &*weak_ptr };

        let mut cur = weak_atomic.load(Relaxed);

        loop {
            // Check if the weak counter is currently "locked"; if so, spin.
            if cur == usize::MAX {
                hint::spin_loop();
                cur = weak_atomic.load(Relaxed);
                continue;
            }

            assert!(cur <= MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");

            match weak_atomic.compare_exchange_weak(cur, cur + 1, Acquire, Relaxed) {
                Ok(_) => {
                    debug_assert!(!is_dangling(self.ptr));
                    return WeakAnyRef { ptr: self.ptr };
                }
                Err(old) => cur = old,
            }
        }
    }

    /// Returns the number of weak references (excluding the implicit one).
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(10);
    /// let w = a.downgrade();
    /// assert_eq!(AnyRef::weak_count(&a), 1);
    /// ```
    #[inline]
    pub fn weak_count(this: &Self) -> usize {
        let cnt = this.inner().weak.load(Relaxed);
        if cnt == usize::MAX { 0 } else { cnt - 1 }
    }

    /// Returns the number of strong references.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new("count");
    /// let b = a.clone();
    /// assert_eq!(AnyRef::strong_count(&a), 2);
    /// ```
    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.load(Relaxed)
    }

    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::addr_eq(this.get_mut_inner_ptr(), other.get_mut_inner_ptr())
    }

    pub fn into_raw(self) -> *const Box<dyn Any> {
        let this = ManuallyDrop::new(self);
        let inner_ptr = this.ptr as *mut AnyRefInner;

        // SAFETY: Get pointer to data field without creating reference to AnyRefInner
        let cell_ptr = unsafe { ptr::addr_of!((*inner_ptr).data) };
        cell_ptr as *const Box<dyn Any>
    }

    pub unsafe fn from_raw<T: ?Sized>(ptr: *const T) -> Self {
        unsafe { Self::from_raw_in(ptr) }
    }

    pub fn type_name(&self) -> &'static str {
        self.inner().type_name
    }
}

impl PtrInterface for AnyRef {
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

impl AnyRef {
    pub fn try_downcast_ref<U: Any>(&self) -> Option<WatchGuardRef<'_, U>> {
        if self.inner().type_id == TypeId::of::<U>() {
            let guard = self.inner().data.lock_shared();
            match unsafe { WatchGuardRef::downcast::<U>(guard) } {
                Ok(guard) => Some(guard),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    pub fn try_downcast_mut<U: Any>(&self) -> Option<WatchGuardMut<'_, U>> {
        if self.inner().type_id == TypeId::of::<U>() {
            let guard = self.inner().data.lock_exclusive();
            match unsafe { WatchGuardMut::downcast::<U>(guard) } {
                Ok(guard) => Some(guard),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    pub fn as_ref<U: Any>(&self) -> WatchGuardRef<'_, U> {
        match self.try_downcast_ref::<U>() {
            Some(data) => data,
            None => panic!("Downcast failed"),
        }
    }

    pub fn as_mut<U: Any>(&self) -> WatchGuardMut<'_, U> {
        match self.try_downcast_mut::<U>() {
            Some(data) => data,
            None => panic!("Downcast mut failed"),
        }
    }
}

impl Clone for AnyRef {
    /// Makes a clone of the `AnyRef` pointer.
    ///
    /// This creates another pointer to the same allocation, increasing the
    /// strong reference count.
    #[inline]
    fn clone(&self) -> AnyRef {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        if self.inner().strong.fetch_add(1, Relaxed) > MAX_REFCOUNT {
            abort();
        }

        unsafe { Self::from_inner_in(self.get_mut_inner_ptr()) }
    }
}

impl Default for AnyRef {
    fn default() -> AnyRef {
        unsafe {
            Self::from_inner(Box::leak(Box::write(
                Box::new_uninit(),
                AnyRefInner::default(),
            )))
        }
    }
}

impl AnyRef {
    /// Creates a new `AnyRef` using the default value of `T`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a: AnyRef = AnyRef::default_with::<String>();
    /// assert_eq!(a.as_ref::<String>(), "");
    /// ```
    pub fn default_with<T: 'static + Default>() -> Self {
        Self::from(Box::new(T::default()))
    }

    /// Replaces the inner value with a new value of type `T`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(0);
    /// let a = AnyRef::fill(a, 123);
    /// assert_eq!(a.as_ref::<i32>(), 123);
    /// ```
    pub fn fill<T: 'static>(mut this: Self, value: T) -> Self {
        let ref_inner = &mut *this.inner_mut();
        let mut wd = ref_inner.data.lock_exclusive();
        *wd = Box::new(value);
        ref_inner.type_id = TypeId::of::<T>();
        drop(wd);
        this
    }
}

impl Drop for AnyRef {
    fn drop(&mut self) {
        let inner_ptr = self.ptr as *mut AnyRefInner;

        // SAFETY: Access the strong count atomically via raw pointer.
        // This avoids creating a reference to the entire AnyRefInner struct.
        let strong_ptr = unsafe { ptr::addr_of!((*inner_ptr).strong) };

        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object.
        if unsafe { &*strong_ptr }.fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence synchronizes with the Release ordering in the fetch_sub above.
        // It ensures that all previous writes to the data are visible before we drop it.
        atomic::fence(Acquire);

        // Create a weak reference that will handle deallocation of AnyRefInner
        // when all weak references are gone.
        let _weak = WeakAnyRef { ptr: self.ptr };

        // SAFETY: We're the last strong reference, so we have exclusive access to the data.
        // We use ptr::addr_of_mut! to get a raw pointer to the `data` field directly,
        // without creating a reference to the entire AnyRefInner struct.
        // This is crucial because WeakAnyRef::upgrade() may concurrently read the atomic
        // fields (strong, weak) of AnyRefInner, and creating a mutable reference to the
        // entire struct would conflict with those reads.
        unsafe {
            let data_ptr = ptr::addr_of_mut!((*inner_ptr).data);
            ptr::drop_in_place(data_ptr);
        }
    }
}

impl<T: 'static> From<*mut T> for AnyRef {
    /// Creates a new `AnyRef` taking possession over the pointed value `*mut T`.
    ///
    /// # Safety
    /// - `ptr` must be valid and pointing to a dynamically allocated instance of T
    ///   (ex. `Box::into_raw`).
    /// - After AnyRef will own `ptr` and no other reference to ptr should be used.
    ///
    /// # Example
    /// ```
    /// use castbox::utils::{create_raw_pointer, dealloc_layout};
    /// use castbox::AnyRef;
    /// let raw = create_raw_pointer(String::from("hello"));
    /// let a = AnyRef::from(raw);
    /// a.as_mut::<String>().push_str(":1");
    /// dealloc_layout(raw);
    /// assert_eq!(a.as_ref::<String>(), String::from("hello:1"));
    /// ```
    #[inline]
    fn from(ptr: *mut T) -> Self {
        let value = unsafe { ptr::read(ptr) };
        AnyRef::new(value)
    }
}

impl From<&str> for AnyRef {
    /// Creates a new `AnyRef` from a `&str`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::from("hello");
    /// assert_eq!(*a.as_ref::<String>(), "hello");
    /// ```
    #[inline]
    fn from(s: &str) -> Self {
        AnyRef::new(s.to_string())
    }
}

impl<T: 'static> From<Box<T>> for AnyRef {
    /// Creates a new `AnyRef` from a `Box<T>`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let boxed = Box::new("hello");
    /// let a = AnyRef::from(boxed);
    ///  assert_eq!(a.as_ref::<&str>(), "hello");
    /// ```
    #[inline]
    fn from(b: Box<T>) -> Self
    where
        T: Sized,
    {
        unsafe { Self::from_inner(Box::leak(Box::new(AnyRefInner::from_box(b)))) }
    }
}

impl fmt::Debug for AnyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner();
        f.debug_struct("AnyRef")
            .field("type", &inner.type_name)
            .field("S", &inner.strong)
            .field("W", &inner.weak)
            .finish()
    }
}

impl fmt::Pointer for AnyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.inner().data.lock_shared().deref(), f)
    }
}
