use crate::anyref::inner::{AnyRefInner, MAX_REFCOUNT};
use crate::anyref::ptr_interface::PtrInterface;
use crate::anyref::weak::{ImplicitWeak, WeakAnyRef};
use crate::utils::is_dangling;
use crossync::sync::{WatchGuardMut, WatchGuardRef};
use std::any::Any;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::{fmt, hint, ptr};

/// A reference-counted pointer to a type-erased, synchronized value.
///
/// All inserted values implement [`Any`] + [`Send`] + [`Sync`]. Clones share
/// the same allocation, value lock, and runtime type information. Reads return
/// [`WatchGuardRef`]; writes return [`WatchGuardMut`]. A guard keeps its lock
/// until it is dropped.
///
/// Use [`Self::try_downcast_ref`] and [`Self::try_downcast_mut`] to handle type
/// mismatches without panicking. These methods still wait for the value lock.
/// [`Self::fill`] can change the value's type for every clone.
///
/// Release guards before requesting conflicting access through this pointer
/// or any clone. Nested access can deadlock. For a statically typed alternative,
/// see [`crate::Arw`].
#[repr(transparent)]
pub struct AnyRef {
    ptr: *const AnyRefInner,
}

// SAFETY: Every insertion path requires Send + Sync. The erased Box<dyn Any>
// is accessed only through its lock, including its type metadata.
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
    ///
    /// Payloads must support ownership transfer and shared access across threads:
    ///
    /// ```compile_fail
    /// castbox::AnyRef::new(std::rc::Rc::new(42));
    /// ```
    ///
    /// ```compile_fail
    /// castbox::AnyRef::new(std::cell::RefCell::new(42));
    /// ```
    pub fn new<T>(value: T) -> Self
    where
        T: Any + Send + Sync,
    {
        unsafe { Self::from_inner(Box::leak(Box::new(AnyRefInner::new(value)))) }
    }

    /// Attempts to extract the inner value if there is exactly one strong
    /// reference, no explicit weak references, and the requested type matches.
    /// Returns the original handle in `Err` when any condition is unmet.
    /// Existing weak handles must be dropped before extraction can succeed.
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

        // Reject explicit weak references before touching the strong count.
        // Lock out concurrent downgrades until we have checked sole ownership,
        // so an unsuccessful unwrap never exposes a transient strong count of 0.
        if unsafe { &*weak_ptr }
            .compare_exchange(1, usize::MAX, Acquire, Relaxed)
            .is_err()
        {
            return Err(this);
        }
        let sole_owner = unsafe { &*strong_ptr }
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_ok();
        unsafe { &*weak_ptr }.store(1, Release);
        if !sole_owner {
            return Err(this);
        }

        // With sole strong ownership and no weak references, replacement cannot
        // race this type check. Keep the original reference on a mismatch.
        if this.try_downcast_ref::<T>().is_none() {
            unsafe { &*strong_ptr }.store(1, Release);
            return Err(this);
        }

        // We will consume `this` on success; keep it alive for now
        let this = ManuallyDrop::new(this);

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
                Err(_) => unreachable!("Type mismatch after guarded downcast check"),
            }
        };

        // Make a weak pointer to clean up the implicit strong-weak reference
        // and deallocate the AnyRefInner when it drops
        let _weak = ImplicitWeak { ptr: this.ptr };

        // Drop the data field - it now contains a dummy Box<()> which is safe to drop
        unsafe {
            let data_field_ptr = ptr::addr_of_mut!((*inner_ptr).data);
            ptr::drop_in_place(data_field_ptr);
            ptr::drop_in_place(ptr::addr_of_mut!((*inner_ptr).type_name));
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

    /// Consumes this handle and maps a read-guarded value into a new allocation.
    ///
    /// Other owners of the original allocation retain their value. The closure
    /// runs while a read lock is held; it must not request write access to the
    /// original allocation. The returned payload must implement `Any + Send + Sync`.
    ///
    /// # Panics
    /// Panics if the stored value is not of type `T`.
    ///
    /// ```compile_fail
    /// let a = castbox::AnyRef::new(42i32);
    /// let _ = a.map(|v: castbox::WatchGuardRef<'_, i32>| std::rc::Rc::new(*v));
    /// ```
    #[inline]
    pub fn map<T, U: Any + Send + Sync, F>(self, func: F) -> AnyRef
    where
        T: Any,
        F: FnOnce(WatchGuardRef<'_, T>) -> U,
    {
        let ptr = self.as_ref::<T>();
        AnyRef::new(func(ptr))
    }

    /// Acquires a read lock and returns a raw pointer to the value of type `T`.
    ///
    /// The lock is released before this method returns. Prefer a value guard
    /// unless a raw pointer is required for interoperability.
    ///
    /// # Panics
    /// Panics if the stored value is not of type `T`.
    ///
    /// # Safety
    /// When dereferencing the returned pointer, the caller must keep a strong
    /// reference alive and prevent concurrent mutation or replacement of the value.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(50);
    /// let ptr = unsafe { a.as_cast_ptr::<i32>() };
    /// unsafe { assert_eq!(*ptr, 50); }
    /// ```
    pub unsafe fn as_cast_ptr<T: Any>(&self) -> *const T {
        let guard = self.as_ref::<T>();
        &*guard as *const T
    }

    /// Checks for one strong reference and no explicit weak references.
    ///
    /// The result does not reserve ownership and can change immediately if
    /// other threads create or drop handles. Use [`Self::try_unwrap`] to extract
    /// a value after checking ownership atomically.
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

    /// Creates a weak handle without consuming this strong reference.
    ///
    /// The weak handle retains reference-counting storage, but does not keep
    /// the value alive. See [`WeakAnyRef::upgrade`].
    ///
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
                    self.inner().explicit_weak.fetch_add(1, Relaxed);
                    debug_assert!(!is_dangling(self.ptr));
                    return WeakAnyRef { ptr: self.ptr };
                }
                Err(old) => cur = old,
            }
        }
    }

    /// Returns the number of weak references (excluding the implicit one).
    /// This count may change immediately when other threads create or drop handles.
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
        this.inner().explicit_weak.load(Relaxed)
    }

    /// Returns the number of strong references.
    /// This count may change immediately when other threads clone or drop handles.
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

    /// Returns whether both handles refer to the same allocation.
    ///
    /// This compares pointer identity without locking or comparing the values.
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::addr_eq(this.get_mut_inner_ptr(), other.get_mut_inner_ptr())
    }

    /// Converts this strong reference into an opaque ownership pointer.
    /// The pointer must only be passed to `AnyRef::from_raw`; it is not a payload pointer.
    /// The strong count is unchanged. Failing to restore the handle leaks its ownership.
    pub fn into_raw(self) -> *const () {
        let this = ManuallyDrop::new(self);
        this.ptr.cast()
    }

    /// Restores a strong reference previously converted with `AnyRef::into_raw`.
    ///
    /// # Safety
    /// `ptr` must be an unconsumed pointer returned by `AnyRef::into_raw`.
    /// Each ownership pointer must be restored exactly once.
    pub unsafe fn from_raw(ptr: *const ()) -> Self {
        unsafe { Self::from_inner_in(ptr.cast_mut().cast()) }
    }

    /// Returns the current value's [`std::any::type_name`] for diagnostics.
    ///
    /// Replacement updates the name for all clones. This method does not acquire
    /// a value lock, so it can be called while holding a value guard. Without a
    /// guard, another thread may replace the value immediately after this call.
    /// The name is not a substitute for a checked downcast.
    pub fn type_name(&self) -> &'static str {
        *self
            .inner()
            .type_name
            .read()
            .unwrap_or_else(|e| e.into_inner())
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
    /// Acquires a read lock and checks whether the value has type `U`.
    ///
    /// Returns a read guard on a match, or `None` on a type mismatch. The type
    /// check and guarded access refer to the same value, even during replacement.
    /// Despite the `try` prefix, this method waits for the read lock.
    pub fn try_downcast_ref<U: Any>(&self) -> Option<WatchGuardRef<'_, U>> {
        let guard = self.inner().data.lock_shared();
        // SAFETY: This guard contains Box<dyn Any>; crossync checks the payload
        // type while the shared lock is held and preserves that lock on success.
        unsafe { WatchGuardRef::downcast::<U>(guard) }.ok()
    }

    /// Acquires a write lock and checks whether the value has type `U`.
    ///
    /// Returns a write guard on a match, or `None` on a type mismatch. Despite
    /// the `try` prefix, this method waits for exclusive access.
    pub fn try_downcast_mut<U: Any>(&self) -> Option<WatchGuardMut<'_, U>> {
        let guard = self.inner().data.lock_exclusive();
        // SAFETY: As above, with an exclusive lock.
        unsafe { WatchGuardMut::downcast::<U>(guard) }.ok()
    }

    /// Acquires a read lock and returns a guard to the value of type `U`.
    ///
    /// # Panics
    /// Panics on a type mismatch. Use [`Self::try_downcast_ref`] to receive `None` instead.
    pub fn as_ref<U: Any>(&self) -> WatchGuardRef<'_, U> {
        match self.try_downcast_ref::<U>() {
            Some(data) => data,
            None => panic!("Downcast failed"),
        }
    }

    /// Acquires a write lock and returns a guard to the value of type `U`.
    ///
    /// Drop the guard before requesting further value access through this
    /// pointer or any clone.
    ///
    /// # Panics
    /// Panics on a type mismatch. Use [`Self::try_downcast_mut`] to receive `None` instead.
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
    /// Creates an `AnyRef` containing `()`.
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
    ///
    /// ```compile_fail
    /// castbox::AnyRef::default_with::<std::cell::RefCell<i32>>();
    /// ```
    pub fn default_with<T: Any + Send + Sync + Default>() -> Self {
        Self::from(Box::new(T::default()))
    }

    /// Replaces the inner value with a new value of type `T`.
    ///
    /// Consumes and returns this handle while preserving allocation identity.
    /// All clones observe the replacement, including its new runtime type and
    /// type name. Waits for exclusive access and drops the previous value after
    /// releasing the lock. If that destructor panics, surviving clones retain
    /// the new value.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::new(0);
    /// let a = AnyRef::fill(a, 123);
    /// assert_eq!(a.as_ref::<i32>(), 123);
    /// ```
    ///
    /// ```compile_fail
    /// let a = castbox::AnyRef::new(42);
    /// castbox::AnyRef::fill(a, std::rc::Rc::new(42));
    /// ```
    ///
    /// ```compile_fail
    /// let a = castbox::AnyRef::new(42);
    /// castbox::AnyRef::fill(a, std::cell::RefCell::new(42));
    /// ```
    pub fn fill<T: Any + Send + Sync>(this: Self, value: T) -> Self {
        let old = {
            let inner = this.inner();
            let mut data = inner.data.lock_exclusive();
            let mut type_name = inner.type_name.write().unwrap_or_else(|e| e.into_inner());
            let old = std::mem::replace(&mut *data, Box::new(value));
            *type_name = std::any::type_name::<T>();
            old
        };
        // User destructors run after releasing the locks, with metadata consistent
        // even if the old value's destructor panics or accesses another clone.
        drop(old);
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
        let _weak = ImplicitWeak { ptr: self.ptr };

        // SAFETY: We're the last strong reference, so we have exclusive access to the data.
        // We use ptr::addr_of_mut! to get a raw pointer to the `data` field directly,
        // without creating a reference to the entire AnyRefInner struct.
        // This is crucial because WeakAnyRef::upgrade() may concurrently read the atomic
        // fields (strong, weak) of AnyRefInner, and creating a mutable reference to the
        // entire struct would conflict with those reads.
        unsafe {
            let data_ptr = ptr::addr_of_mut!((*inner_ptr).data);
            ptr::drop_in_place(data_ptr);
            ptr::drop_in_place(ptr::addr_of_mut!((*inner_ptr).type_name));
        }
    }
}

impl AnyRef {
    /// Takes ownership of an initialized value and its `Box`-compatible allocation.
    ///
    /// This accepts a payload pointer, not an ownership pointer produced by
    /// [`Self::into_raw`]. Prefer [`Self::new`] or `From<Box<T>>` when possible.
    ///
    /// # Safety
    /// `ptr` must be a valid, unconsumed pointer returned by `Box::into_raw`
    /// or `create_raw_pointer` for the same `T`. This call takes exclusive
    /// ownership of both the value and its allocation; do not access or free it again.
    ///
    /// # Example
    /// ```
    /// use castbox::utils::create_raw_pointer;
    /// use castbox::AnyRef;
    /// let raw = create_raw_pointer(String::from("hello"));
    /// let a = unsafe { AnyRef::from_owned_raw(raw) };
    /// a.as_mut::<String>().push_str(":1");
    /// assert_eq!(a.as_ref::<String>(), String::from("hello:1"));
    /// ```
    ///
    /// ```compile_fail
    /// let raw = castbox::utils::create_raw_pointer(42);
    /// castbox::AnyRef::from_owned_raw(raw); // requires an unsafe block
    /// ```
    ///
    /// ```compile_fail
    /// castbox::AnyRef::from(std::ptr::null_mut::<i32>()); // no safe pointer conversion
    /// ```
    ///
    /// ```compile_fail
    /// let raw = Box::into_raw(Box::new(std::rc::Rc::new(42)));
    /// unsafe { castbox::AnyRef::from_owned_raw(raw) }; // still requires Send + Sync
    /// ```
    #[inline]
    pub unsafe fn from_owned_raw<T: Any + Send + Sync>(ptr: *mut T) -> Self {
        // SAFETY: The caller transfers a Box-compatible allocation and its value.
        Self::from(unsafe { Box::from_raw(ptr) })
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

impl<T: Any + Send + Sync> From<Box<T>> for AnyRef {
    /// Creates a new `AnyRef` from a `Box<T>`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let boxed = Box::new("hello");
    /// let a = AnyRef::from(boxed);
    ///  assert_eq!(a.as_ref::<&str>(), "hello");
    /// ```
    ///
    /// ```compile_fail
    /// castbox::AnyRef::from(Box::new(std::rc::Rc::new(42)));
    /// ```
    ///
    /// ```compile_fail
    /// castbox::AnyRef::from(Box::new(std::cell::RefCell::new(42)));
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
            .field("type", &self.type_name())
            .field("S", &inner.strong)
            .field("W", &inner.weak)
            .finish()
    }
}

impl fmt::Pointer for AnyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr, f)
    }
}
