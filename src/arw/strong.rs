use crate::arw::inner::{ArwInner, MAX_REFCOUNT};
use crate::arw::ptr_interface::PtrInterface;
use crate::arw::WeakArw;
use crate::utils::is_dangling;
use crossync::sync::{RwLock, WatchGuardMut, WatchGuardRef};
use std::any::Any;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
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
    pub fn new(value: T) -> Self
    where
        T: Any,
    {
        unsafe { Self::from_inner(Box::leak(Box::new(ArwInner::new(value)))) }
    }

    /// Attempts to extract the inner value if there is exactly one strong reference
    /// and no weak references.
    pub fn try_unwrap(this: Self) -> Result<T, Self>
    where
        T: 'static,
    {
        // Try to transition strong 1 -> 0
        if this
            .inner()
            .strong
            .compare_exchange(1, 0, Acquire, Relaxed)
            .is_err()
        {
            return Err(this);
        }

        atomic::fence(Acquire);

        // Check weak count
        let weak_count = this.inner().weak.load(Relaxed);
        if weak_count != 1 {
            this.inner().strong.store(1, Release);
            return Err(this);
        }

        // Prevent Drop from running
        let this = ManuallyDrop::new(this);
        let inner_ptr = this.ptr as *mut ArwInner<T>;

        unsafe {
            // Get exclusive access and take the value out of ManuallyDrop
            let mut guard = (*inner_ptr).val.lock_exclusive();
            let elem: T = ManuallyDrop::take(&mut *guard);
            drop(guard);

            // Now drop the RwLock (it contains ManuallyDrop which won't drop T)
            ptr::drop_in_place(ptr::addr_of_mut!((*inner_ptr).val));

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

    /// Returns a reference to the inner value.
    pub fn as_ref(&self) -> impl Deref<Target = T> + '_ {
        ArwGuardRef(self.inner().val.lock_shared())
    }

    /// Returns a mutable reference to the inner value.
    pub fn as_mut(&self) -> impl DerefMut<Target = T> + '_ {
        ArwGuardMut(self.inner().val.lock_exclusive())
    }

    /// Returns `true` if this is the only strong reference.
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

    /// Creates a weak reference.
    pub fn downgrade(&self) -> WeakArw<T> {
        let mut cur = self.inner().weak.load(Relaxed);

        loop {
            if cur == usize::MAX {
                hint::spin_loop();
                cur = self.inner().weak.load(Relaxed);
                continue;
            }

            assert!(cur <= MAX_REFCOUNT, "INTERNAL OVERFLOW ERROR");

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

    #[inline]
    pub fn weak_count(this: &Self) -> usize {
        let cnt = this.inner().weak.load(Relaxed);
        if cnt == usize::MAX { 0 } else { cnt - 1 }
    }

    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.load(Relaxed)
    }

    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::addr_eq(this.get_mut_inner_ptr(), other.get_mut_inner_ptr())
    }

    pub fn into_raw(self) -> *const T {
        let this = ManuallyDrop::new(self);
        let inner: *mut ArwInner<T> = this.get_mut_inner_ptr();
        let cell_ptr: *const RwLock<ManuallyDrop<T>> = unsafe { ptr::addr_of!((*inner).val) };
        cell_ptr.cast::<T>()
    }

    pub unsafe fn from_raw(ptr: *const T) -> Self {
        unsafe { Self::from_raw_in(ptr) }
    }
}

// Wrapper per nascondere ManuallyDrop all'utente
pub struct ArwGuardRef<'a, T>(WatchGuardRef<'a, ManuallyDrop<T>>);

impl<'a, T> Deref for ArwGuardRef<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<'a, T> ArwGuardRef<'a, T> {
    pub fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

pub struct ArwGuardMut<'a, T>(WatchGuardMut<'a, ManuallyDrop<T>>);

impl<'a, T> Deref for ArwGuardMut<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<'a, T> DerefMut for ArwGuardMut<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

impl<'a, T> ArwGuardMut<'a, T> {
    pub fn is_locked(&self) -> bool {
        self.0.is_locked()
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
    #[inline]
    fn clone(&self) -> Arw<T> {
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
    /// Replaces the inner value.
    pub fn fill(this: Self, value: T) -> Self {
        let ptr: *mut ArwInner<T> = this.get_mut_inner_ptr();
        let ref_inner = unsafe { &mut *ptr };
        let mut wd = ref_inner.val.lock_exclusive();
        *wd = ManuallyDrop::new(value);
        drop(wd);
        this
    }
}

impl<T> Drop for Arw<T>
where
    T: Sized,
{
    fn drop(&mut self) {
        if self.inner().strong.fetch_sub(1, Release) != 1 {
            return;
        }

        atomic::fence(Acquire);

        // Drop the inner T manually
        unsafe {
            let val_ptr = ptr::addr_of_mut!((*self.get_mut_inner_ptr()).val);
            let mut guard = (*val_ptr).lock_exclusive();
            ManuallyDrop::drop(&mut *guard);
            drop(guard);

            // Now drop the RwLock itself
            ptr::drop_in_place(val_ptr);
        }

        // Create weak to handle deallocation
        let _weak = WeakArw { ptr: self.ptr };
    }
}

impl<T: Sized + 'static> From<*mut T> for Arw<T> {
    #[inline]
    fn from(ptr: *mut T) -> Self {
        assert!(!ptr.is_null(), "pointer must not be null");
        let value = unsafe { ptr::read(ptr) };
        Arw::new(value)
    }
}

impl From<&str> for Arw<String> {
    #[inline]
    fn from(s: &str) -> Self {
        Arw::new(s.to_string())
    }
}

impl<T: 'static> From<Box<T>> for Arw<Box<T>> {
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
        let guard = self.inner().val.lock_shared();
        fmt::Pointer::fmt(&(&**guard as *const T), f)
    }
}
