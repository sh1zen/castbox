use crate::any_ref::inner::{AnyRefInner, MAX_REFCOUNT};
use crate::any_ref::ptr_interface::PtrInterface;
use crate::any_ref::weak::WeakAnyRef;
use crate::mutex::{WatchGuardMut, WatchGuardRef};
use crate::utils::is_dangling;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
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
    pub fn try_unwrap<T>(this: Self) -> Result<T, Self> {
        this.inner().lock.lock_exclusive();
        if this
            .inner()
            .strong
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_err()
        {
            this.inner().lock.unlock_exclusive();
            return Err(this);
        }

        atomic::fence(Acquire);

        let this = ManuallyDrop::new(this);
        let this_data = unsafe { &**(*this.ptr).data.get() as *const _ };
        let elem: T = unsafe { ptr::read(this_data as *const T) };

        // Make a weak pointer to clean up the implicit strong-weak reference
        let _weak = WeakAnyRef { ptr: this.ptr };

        unsafe { ptr::drop_in_place(&mut (*this.get_mut_inner_ptr()).data) }
        unsafe { ptr::drop_in_place(&mut (*this.get_mut_inner_ptr()).lock) }

        Ok(elem)
    }

    pub(crate) fn inner(&self) -> &AnyRefInner {
        // This unsafety is ok because while this AnyRef is alive we're guaranteed
        // that the inner pointer is valid.
        let ptr: *const AnyRefInner = self.ptr;
        unsafe { &*ptr }
    }

    fn inner_mut(&mut self) -> &mut AnyRefInner {
        let ptr: *mut AnyRefInner = self.get_mut_inner_ptr();
        unsafe { &mut *ptr }
    }

    pub fn is_locked(&self) -> bool {
        self.inner().lock.is_locked_exclusive()
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
        // lock the weak pointer count if we appear to be the sole weak pointer
        // holder.
        //
        // The acquire label here ensures a happens-before relationship with any
        // writes to `strong` (in particular in `Weak::upgrade`) prior to decrements
        // of the `weak` count (via `Weak::drop`, which uses release). If the upgraded
        // weak ref was never dropped, the CAS here will fail so we do not care to synchronize.
        if this
            .inner()
            .weak
            .compare_exchange(1, usize::MAX, Acquire, Relaxed)
            .is_ok()
        {
            // This needs to be an `Acquire` to synchronize with the decrement of the `strong`
            // counter in `drop` -- the only access that happens when any but the last reference
            // is being dropped.
            let unique = this.inner().strong.load(Acquire) == 1;

            // The release write here synchronizes with a read in `downgrade`,
            // effectively preventing the above read of `strong` from happening
            // after the write.
            this.inner().weak.store(1, Release); // release the lock
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
        // This Relaxed is OK because we're checking the value in the CAS
        // below.
        let mut cur = self.inner().weak.load(Relaxed);

        loop {
            // check if the weak counter is currently "locked"; if so, spin.
            if cur == usize::MAX {
                hint::spin_loop();
                cur = self.inner().weak.load(Relaxed);
                continue;
            }

            // We can't allow the refcount to increase much past `MAX_REFCOUNT`.
            assert!(cur <= MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");

            // NOTE: this code currently ignores the possibility of overflow
            // into usize::MAX; in general both Rc and AnyRef need to be adjusted
            // to deal with overflow.

            // Unlike with Clone(), we need this to be an Acquire read to
            // synchronize with the write coming from `is_unique`, so that the
            // events prior to that write happen before this read.
            match self
                .inner()
                .weak
                .compare_exchange_weak(cur, cur + 1, Acquire, Relaxed)
            {
                Ok(_) => {
                    // Make sure we do not create a dangling Weak
                    debug_assert!(!is_dangling(self.inner()));
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
        // If the weak count is currently locked, the value of the
        // count was 0 just before taking the lock.
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
        // prevent auto drop
        let this = ManuallyDrop::new(self);

        let inner: *mut AnyRefInner = this.get_mut_inner_ptr();

        // Make sure Miri realizes that we transition from a noalias pointer to a raw pointer here.
        let cell_ptr: *const UnsafeCell<Box<dyn Any>> = unsafe { ptr::addr_of!((*inner).data) };
        let data_ptr: *const Box<dyn Any> = cell_ptr.cast::<Box<dyn Any>>();

        data_ptr
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
            let lock = self.inner().lock.clone();
            lock.lock_group();

            let data = self.inner().get_ref();
            let data = data.downcast_ref::<U>();

            match data {
                Some(t) => Some(WatchGuardRef::new(t, lock)),
                None => {
                    lock.unlock_group();
                    None
                }
            }
        } else {
            None
        }
    }

    pub fn try_downcast_mut<U: Any>(&self) -> Option<WatchGuardMut<'_, U>> {
        if self.inner().type_id == TypeId::of::<U>() {
            let lock = self.inner().lock.clone();
            lock.lock_exclusive();

            let data = self.inner().get_mut_ref().downcast_mut::<U>();
            match data {
                Some(t) => Some(WatchGuardMut::new(t, lock)),
                None => {
                    lock.unlock_exclusive();
                    None
                }
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
    /// use castbox::{AnyRef};
    /// let a = AnyRef::new(0);
    /// let a = AnyRef::fill(a, 123);
    /// assert_eq!(a.as_ref::<i32>(), 123);
    /// ```
    pub fn fill<T: 'static>(mut this: Self, value: T) -> Self {
        let ref_inner = &mut *this.inner_mut();
        ref_inner.lock.lock_exclusive();
        ref_inner.data = UnsafeCell::new(Box::new(value));
        ref_inner.type_id = TypeId::of::<T>();
        ref_inner.lock.unlock_exclusive();
        this
    }
}

impl Drop for AnyRef {
    fn drop(&mut self) {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object. This
        // same logic applies to the below `fetch_sub` to the `weak` count.
        if self.inner().strong.fetch_sub(1, Release) != 1 {
            return;
        }

        atomic::fence(Acquire);

        let _weak = WeakAnyRef { ptr: self.ptr };

        unsafe { ptr::drop_in_place(&mut (*self.get_mut_inner_ptr()).lock) }

        unsafe { ptr::drop_in_place(&mut (*self.get_mut_inner_ptr()).data) }
    }
}

impl<T: 'static> From<*mut T> for AnyRef {
    /// Creates a new `AnyRef` taking posses over the pointed value `*mut T`.
    ///
    /// # Safety
    /// - `ptr` must be valid and pointing to a dynamically allocated instance of T
    ///   (ex. `Box::into_raw`).
    /// - After AnyRef will own `ptr` and no other reference to ptr should be used.
    ///
    /// # Example
    /// ```
    /// use castbox::utils::{create_raw_pointer, dealloc_layout};
    /// use castbox::{AnyRef};
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
    /// Creates a new `AnyRef` from a `*const T`.
    ///
    /// # Example
    /// ```
    /// use castbox::AnyRef;
    /// let a = AnyRef::from("hello");
    /// assert_eq!(*a.as_ref::<String>(), "hello");
    /// ```
    #[inline]
    fn from(s: &str) -> Self {
        // copy data to own them
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
        fmt::Pointer::fmt(&self.inner().data.get(), f)
    }
}
