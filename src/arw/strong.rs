use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::arw::WeakArw;
use crate::utils::is_dangling;
use crossync::sync::{RwLock, WatchGuardMut, WatchGuardRef};
use std::any::Any;
use std::mem::ManuallyDrop;
use std::ops::Deref;
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
    pub fn try_unwrap(this: Self) -> Result<T, Self>
    where
        T: 'static,
    {
        // Try to transition strong 1 -> 0 to become sole strong owner.
        if this
            .inner()
            .strong
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_err()
        {
            return Err(this);
        }

        // synchronize with any release operations
        atomic::fence(Acquire);

        // Check weak count. If it's not the implicit weak (1), restore and return Err.
        let weak_count = this.inner().weak.load(Relaxed);
        if weak_count != 1 {
            this.inner().strong.store(1, Release);
            return Err(this);
        }

        // Prevent Drop from running
        let this = ManuallyDrop::new(this);
        let inner_ptr = this.ptr as *mut ArwInner<T>;

        unsafe {
            // Extract the value from the RwLock
            let guard = (*inner_ptr).val.lock_exclusive();
            let elem: T = ptr::read(&*guard);
            drop(guard);

            // We need to drop the RwLock's internals but NOT the T value.
            // Since RwLock<T> contains T directly, we can't use drop_in_place.
            // Instead, we manually read and drop the RwLock, then forget the T part.
            let rwlock = ptr::read(ptr::addr_of!((*inner_ptr).val));

            // Convert RwLock<T> to RwLock<ManuallyDrop<T>> conceptually
            // by transmuting and then forgetting the inner value
            let rwlock_md: RwLock<ManuallyDrop<T>> = std::mem::transmute(rwlock);
            drop(rwlock_md); // This drops RwLock's internals but not T

            // Decrement weak and deallocate
            let weak_ptr = ptr::addr_of!((*inner_ptr).weak);
            if (*weak_ptr).fetch_sub(1, Release) == 1 {
                atomic::fence(Acquire);
                std::alloc::dealloc(
                    inner_ptr as *mut u8,
                    std::alloc::Layout::new::<ArwInner<T>>(),
                );
            }

            Ok(elem)
        }
    }

    #[inline]
    fn inner(&self) -> &ArwInner<T> {
        // This unsafety is ok because while this Arw is alive we're guaranteed
        // that the inner pointer is valid.
        unsafe { &*self.ptr }
    }

    pub fn map<U: 'static, F>(self, func: F) -> Arw<U>
    where
        T: Any,
        F: FnOnce(&T) -> U,
    {
        let guard = self.as_ref();
        Arw::new(func(&*guard))
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
        self.inner().val.lock_shared()
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
        self.inner().val.lock_exclusive()
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
        if this
            .inner()
            .weak
            .compare_exchange(1, usize::MAX, Acquire, Relaxed)
            .is_ok()
        {
            let unique = this.inner().strong.load(Acquire) == 1;
            this.inner().weak.store(1, Release);
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
        let cell_ptr: *const RwLock<T> = unsafe { ptr::addr_of!((*inner).val) };
        let data_ptr: *const T = cell_ptr.cast::<T>();
        data_ptr.cast::<T>()
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
        let ptr: *mut ArwInner<T> = this.get_mut_inner_ptr();
        let ref_inner = unsafe { &mut *ptr };
        let mut wd = ref_inner.val.lock_exclusive();
        *wd = value;
        drop(wd);
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

        // Create weak to handle deallocation
        let _weak = WeakArw { ptr: self.ptr };

        unsafe {
            // Now drop the RwLock itself
            ptr::drop_in_place(ptr::addr_of_mut!((*self.get_mut_inner_ptr()).val))
        };
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
        assert!(!ptr.is_null(), "pointer must not be null");
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
    fn from(b: Box<T>) -> Self {
        Arw::new(b)
    }
}

impl<T> fmt::Debug for Arw<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arw")
            .field("S", &self.inner().strong)
            .field("W", &self.inner().weak)
            .finish()
    }
}

impl<T> fmt::Pointer for Arw<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.inner().val.lock_shared().deref(), f)
    }
}
