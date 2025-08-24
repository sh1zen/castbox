use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::arw::WeakArw;
use crate::mutex::{WatchGuardMut, WatchGuardRef};
use crate::utils::is_dangling;
use std::any::Any;
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::{fmt, hint, ptr};

#[repr(transparent)]
pub struct Arw<T: Sized> {
    ptr: *const ArwInner<T>,
}

unsafe impl<T: Sized + Sync + Send> Send for Arw<T> {}
unsafe impl<T: Sized + Sync + Send> Sync for Arw<T> {}

impl<T: Sized> UnwindSafe for Arw<T> {}
impl<T: Sized> RefUnwindSafe for Arw<T> {}
impl<T> Arw<T> {
    /// Creates a new `Arw` containing the given value.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(42);
    /// assert_eq!(a.as_ref(), 42);
    /// ```
    pub fn new(value: T) -> Self
    where
        T: Any,
    {
        unsafe { Self::from_inner(Box::leak(Box::new(ArwInner::new(value)))) }
    }

    /// Attempts to extract the inner value if there is exactly one strong reference.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(123i32);
    /// let value = Arw::try_unwrap(a).unwrap();
    /// assert_eq!(value, 123i32);
    /// ```
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
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
        let elem: T = unsafe { this.read_data() };

        // Make a weak pointer to clean up the implicit strong-weak reference
        let _weak = WeakArw { ptr: this.ptr };

        unsafe { ptr::drop_in_place(&mut (*this.get_mut_inner_ptr()).val) }
        unsafe { ptr::drop_in_place(&mut (*this.get_mut_inner_ptr()).lock) }

        Ok(elem)
    }

    fn inner(&self) -> &ArwInner<T> {
        // This unsafety is ok because while this Arw is alive we're guaranteed
        // that the inner pointer is valid.
        let ptr: *const ArwInner<T> = self.ptr;
        unsafe { &*ptr }
    }

    fn inner_mut(&self) -> &mut ArwInner<T> {
        let ptr: *mut ArwInner<T> = self.get_mut_inner_ptr();
        unsafe { &mut *ptr }
    }

    pub fn is_locked(&self) -> bool {
        self.inner().lock.is_locked_exclusive()
    }

    #[inline]
    pub fn map<U: 'static, F>(self, func: F) -> Arw<U>
    where
        T: Any,
        F: FnOnce(WatchGuardRef<'_, T>) -> U,
    {
        Arw::new(func(self.as_ref()))
    }

    /// Returns a reference to the inner value of type `T`.
    /// Panics if the type does not match `T`.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(3.14f32);
    /// let f = a.as_ref();
    /// assert_eq!(*f, 3.14f32);
    /// ```
    pub fn as_ref(&self) -> WatchGuardRef<'_, T> {
        let lock = self.inner().lock.clone();
        lock.lock_group();

        WatchGuardRef::new(self.inner().get_ref(), lock)
    }

    /// Returns a mutable reference to the inner value of type `T`.
    /// Panics if the type does not match `T`.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(3i32);
    /// {
    ///     let mut f = a.as_mut();
    ///     *f += 3i32;
    /// }
    /// assert_eq!(*a.as_ref(), 6i32);
    /// ```
    pub fn as_mut(&self) -> WatchGuardMut<'_, T> {
        let lock = self.inner().lock.clone();
        lock.lock_exclusive();

        WatchGuardMut::new(self.inner().get_mut_ref(), lock)
    }

    /// Returns `true` if the `Arw` is the only strong reference to the value.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new("unique");
    /// assert!(Arw::is_unique(&a));
    /// let b = a.clone();
    /// assert!(!Arw::is_unique(&a));
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
    /// use castbox::Arw;
    /// let five = Arw::new(5);
    /// let weak_five = Arw::downgrade(&five);
    /// ```
    pub fn downgrade(&self) -> WeakArw<T> {
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
            // into usize::MAX; in general both Rc and Arw need to be adjusted
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
                    return WeakArw { ptr: self.ptr };
                }
                Err(old) => cur = old,
            }
        }
    }

    /// Returns the number of weak references (excluding the implicit one).
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(10);
    /// let w = a.downgrade();
    /// assert_eq!(Arw::weak_count(&a), 1);
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
    /// use castbox::Arw;
    /// let a = Arw::new("count");
    /// let b = a.clone();
    /// assert_eq!(Arw::strong_count(&a), 2);
    /// ```
    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.load(Relaxed)
    }

    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::addr_eq(this.get_mut_inner_ptr(), other.get_mut_inner_ptr())
    }

    pub fn into_raw(self) -> *const T {
        // prevent auto drop
        let this = ManuallyDrop::new(self);

        let inner: *mut ArwInner<T> = this.get_mut_inner_ptr();

        // Make sure Miri realizes that we transition from a noalias pointer to a raw pointer here.
        let cell_ptr: *const UnsafeCell<T> = unsafe { ptr::addr_of!((*inner).val) };
        let data_ptr: *const T = cell_ptr.cast::<T>();

        data_ptr
    }

    pub unsafe fn from_raw(ptr: *const T) -> Self {
        unsafe { Self::from_raw_in(ptr) }
    }
}

impl<T> PtrInterface<T> for Arw<T> {
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

impl<T> Clone for Arw<T> {
    /// Makes a clone of the `Arw` pointer.
    ///
    /// This creates another pointer to the same allocation, increasing the
    /// strong reference count.
    #[inline]
    fn clone(&self) -> Arw<T> {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        if self.inner().strong.fetch_add(1, Relaxed) >= MAX_REFCOUNT {
            abort();
        }

        unsafe { Self::from_inner_in(self.get_mut_inner_ptr()) }
    }
}

impl<T: Default> Default for Arw<T> {
    fn default() -> Arw<T> {
        unsafe {
            Self::from_inner(Box::leak(Box::write(
                Box::new_uninit(),
                ArwInner::default(),
            )))
        }
    }
}

impl<T: Sized> Arw<T> {
    /// Replaces the inner value with a new value of type `T`.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(0);
    /// let a = Arw::fill(a, 123);
    /// assert_eq!(a.as_ref(), 123);
    /// ```
    pub fn fill(this: Self, value: T) -> Self {
        let ref_inner = &mut *this.inner_mut();
        ref_inner.lock.lock_exclusive();
        ref_inner.val = UnsafeCell::new(value);
        ref_inner.lock.unlock_exclusive();
        this
    }
}

impl<T> Drop for Arw<T>
where
    T: Sized,
{
    fn drop(&mut self) {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object. This
        // same logic applies to the below `fetch_sub` to the `weak` count.
        if self.inner().strong.fetch_sub(1, Release) != 1 {
            return;
        }

        atomic::fence(Acquire);

        let _weak = WeakArw { ptr: self.ptr };

        unsafe { ptr::drop_in_place(&mut (*self.get_mut_inner_ptr()).lock) }

        unsafe { ptr::drop_in_place(&mut (*self.get_mut_inner_ptr()).val) }
    }
}

impl<T: Sized + 'static> From<*mut T> for Arw<T> {
    /// Creates a new `Arw` taking posses over the pointed value `*mut T`.
    ///
    /// # Safety
    /// - `ptr` must be valid and pointing to a dynamically allocated instance of T
    ///   (ex. `Box::into_raw`).
    /// - After Arw will own `ptr` and no other reference to ptr should be used.
    ///
    /// # Example
    /// ```
    /// use castbox::utils::{create_raw_pointer, dealloc_layout};
    /// use castbox::Arw;
    /// let raw = create_raw_pointer(String::from("hello"));
    /// let a = Arw::from(raw);
    /// a.as_mut().push_str(":1");
    /// dealloc_layout(raw);
    /// assert_eq!(a.as_ref(), "hello:1");
    /// ```
    #[inline]
    fn from(ptr: *mut T) -> Self {
        let value = unsafe { ptr::read(ptr) };
        Arw::new(value)
    }
}

impl From<&str> for Arw<String> {
    /// Creates a new `Arw` from a `*const T`.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::from("hello");
    /// assert_eq!(*a.as_ref(), "hello");
    /// ```
    #[inline]
    fn from(s: &str) -> Self {
        // copy data to own them
        Arw::new(s.to_string())
    }
}

impl<T: 'static> From<Box<T>> for Arw<Box<T>> {
    /// Creates a new `Arw` from a `Box<T>`.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let boxed = Box::new("hello");
    /// let a = Arw::from(boxed);
    /// assert_eq!(**a.as_ref(), "hello");
    /// ```
    #[inline]
    fn from(b: Box<T>) -> Self
    where
        T: Sized,
    {
        Arw::new(b)
    }
}

impl<T> fmt::Debug for Arw<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner();
        f.debug_struct("Arw")
            .field("S", &inner.strong)
            .field("W", &inner.weak)
            .field("locked", &inner.lock.is_locked_exclusive())
            .finish()
    }
}

impl<T> fmt::Pointer for Arw<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.inner().val.get(), f)
    }
}
