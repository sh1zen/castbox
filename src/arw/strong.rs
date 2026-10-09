use crate::arw::WeakArw;
use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::arw::weak::ImplicitWeak;
use crate::utils::is_dangling;
use crossync::sync::{RwLock, WatchGuardMut, WatchGuardRef};
use std::any::Any;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process::abort;
use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::{fmt, hint, ptr};

/// A reference-counted pointer with read-write locking for a value of type `T`.
///
/// Clones share the same value and allocation. [`Self::as_ref`] returns a
/// [`WatchGuardRef`] for shared access; [`Self::as_mut`] returns a
/// [`WatchGuardMut`] for exclusive access. Each guard holds its lock until dropped.
///
/// `Arw<T>` implements [`Send`] and [`Sync`] when `T` implements both traits.
/// Values without these bounds can be stored for use within one thread.
/// Release guards before requesting conflicting access through any clone;
/// nested access can deadlock. See [`crate::AnyRef`] for runtime type erasure.
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
    /// The value must be `'static`; it need not implement `Send` or `Sync`
    /// unless the pointer is shared across threads.
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

    /// Attempts to extract the inner value if there is exactly one strong
    /// reference and no explicit weak references.
    /// Returns the original handle in `Err` when either condition is unmet.
    /// Existing weak handles must be dropped before extraction can succeed.
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
        // Lock out downgrades and reject existing weak references before touching
        // the strong count. Failed unwraps must not appear dead to weak handles.
        if this
            .inner()
            .weak
            .compare_exchange(1, usize::MAX, Acquire, Relaxed)
            .is_err()
        {
            return Err(this);
        }
        let sole_owner = this
            .inner()
            .strong
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_ok();
        this.inner().weak.store(1, Release);
        if !sole_owner {
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

    /// Consumes this handle and maps a read-locked value into a new allocation.
    ///
    /// Other owners of the original allocation retain their value. The closure
    /// receives `&T` while the read lock is held; it must not request write access
    /// to the original allocation.
    pub fn map<U: 'static, F>(self, func: F) -> Arw<U>
    where
        T: Any,
        F: FnOnce(&T) -> U,
    {
        let guard = self.as_ref();
        Arw::new(func(&*guard))
    }

    /// Waits for a read lock and returns a guard to the value.
    ///
    /// Other read guards may coexist. Drop the guard before requesting write
    /// access through this pointer or a clone.
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

    /// Waits for a write lock and returns a guard for exclusive value access.
    ///
    /// Drop the guard before requesting further value access through this
    /// pointer or any clone.
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

    /// Checks for one strong reference and no explicit weak references.
    ///
    /// The result does not reserve ownership and can change immediately if
    /// other threads create or drop handles. Use [`Self::try_unwrap`] to extract
    /// a value after checking ownership atomically.
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

    /// Creates a weak handle without consuming this strong reference.
    ///
    /// The weak handle retains reference-counting storage, but does not keep
    /// the value alive. See [`WeakArw::upgrade`].
    ///
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
                    self.inner().explicit_weak.fetch_add(1, Relaxed);
                    debug_assert!(!is_dangling(self.inner()));
                    return WeakArw { ptr: self.ptr };
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
    /// use castbox::Arw;
    /// let a = Arw::new(10);
    /// let w = a.downgrade();
    /// assert_eq!(Arw::weak_count(&a), 1);
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
    /// use castbox::Arw;
    /// let a = Arw::new("count");
    /// let b = a.clone();
    /// assert_eq!(Arw::strong_count(&a), 2);
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
    /// It must only be passed to `Arw::<T>::from_raw`; it is not a payload pointer.
    /// The strong count is unchanged. Failing to restore the handle leaks its ownership.
    pub fn into_raw(self) -> *const () {
        let this = ManuallyDrop::new(self);
        this.ptr.cast()
    }

    /// Restores a strong reference converted with `Arw::<T>::into_raw`.
    ///
    /// # Safety
    /// `ptr` must be an unconsumed pointer returned by `Arw::<T>::into_raw`
    /// for exactly the same `T`. Each ownership pointer must be restored once.
    pub unsafe fn from_raw(ptr: *const ()) -> Self {
        unsafe { Self::from_inner_in(ptr.cast_mut().cast()) }
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
    /// Creates an `Arw<T>` containing `T::default()`.
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
    /// Consumes and returns this handle while preserving allocation identity.
    /// All clones observe the replacement. Waits for exclusive access and drops
    /// the previous value after releasing the lock. If that destructor panics,
    /// surviving clones retain the new value.
    ///
    /// # Example
    /// ```
    /// use castbox::Arw;
    /// let a = Arw::new(0);
    /// let a = Arw::fill(a, 123);
    /// assert_eq!(a.as_ref(), 123);
    /// ```
    pub fn fill(this: Self, value: T) -> Self {
        let old = {
            let mut wd = this.inner().val.lock_exclusive();
            std::mem::replace(&mut *wd, value)
        };
        drop(old);
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
        let _weak = ImplicitWeak { ptr: self.ptr };

        unsafe {
            // Now drop the RwLock itself
            ptr::drop_in_place(ptr::addr_of_mut!((*self.get_mut_inner_ptr()).val))
        };
    }
}

impl<T: 'static> Arw<T> {
    /// Takes ownership of an initialized value and its `Box`-compatible allocation.
    ///
    /// This accepts a payload pointer, not an ownership pointer produced by
    /// [`Self::into_raw`]. Prefer [`Self::new`] when possible.
    ///
    /// # Safety
    /// `ptr` must be a valid, unconsumed pointer returned by `Box::into_raw`
    /// or `create_raw_pointer` for the same `T`. This call takes exclusive
    /// ownership of both the value and its allocation; do not access or free it again.
    ///
    /// # Example
    /// ```
    /// use castbox::utils::create_raw_pointer;
    /// use castbox::Arw;
    /// let raw = create_raw_pointer(String::from("hello"));
    /// let a = unsafe { Arw::from_owned_raw(raw) };
    /// a.as_mut().push_str(":1");
    /// assert_eq!(a.as_ref(), "hello:1");
    /// ```
    ///
    /// ```compile_fail
    /// let raw = castbox::utils::create_raw_pointer(42);
    /// castbox::Arw::from_owned_raw(raw); // requires an unsafe block
    /// ```
    ///
    /// ```compile_fail
    /// castbox::Arw::from(std::ptr::null_mut::<i32>()); // no safe pointer conversion
    /// ```
    #[inline]
    pub unsafe fn from_owned_raw(ptr: *mut T) -> Self {
        // SAFETY: The caller transfers a Box-compatible allocation and its value.
        Arw::new(*unsafe { Box::from_raw(ptr) })
    }
}

impl From<&str> for Arw<String> {
    /// Copies a string slice into a new `Arw<String>`.
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
    /// Stores a box as the value of a new `Arw<Box<T>>`.
    ///
    /// The box is retained as the payload. To store `T` directly, use `Arw::new(*boxed)`.
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
        fmt::Pointer::fmt(&self.ptr, f)
    }
}
