use crate::mutex::{Mutex, WatchGuardMut, WatchGuardRef};
use std::cell::UnsafeCell;
use std::mem::{self, MaybeUninit};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::{self, AtomicUsize, Ordering};

/// Internal representation of the atomic cell.
/// Provides interior mutability through a Mutex.
/// Reference counting allows cloning the AtomicCell.
struct Inner<T> {
    /// Value stored in the cell, possibly uninitialized
    val: UnsafeCell<MaybeUninit<T>>,
    /// Mutex protecting concurrent access
    state: Mutex,
    /// Reference count for clone/drop semantics
    ref_count: AtomicUsize,
}

impl<T> Inner<T> {
    /// Creates a new Inner cell containing `val`
    fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(MaybeUninit::new(val)),
            state: Mutex::new(),
            ref_count: AtomicUsize::new(1),
        }
    }
}

// Interior mutability: safe only because access is protected by the lock
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Sync> Sync for Inner<T> {}

// Safe for unwind scenarios
impl<T> UnwindSafe for Inner<T> {}
impl<T> RefUnwindSafe for Inner<T> {}

/// Thread-safe atomic cell wrapper
#[repr(transparent)]
pub struct AtomicCell<T> {
    ptr: *const Inner<T>,
}

impl<T> AtomicCell<T> {
    /// Create a new atomic cell containing `val`
    pub fn new(val: T) -> Self {
        let inner = Box::new(Inner::new(val));
        let ptr = Box::into_raw(inner);
        Self { ptr }
    }

    /// Returns a reference to the inner struct
    #[inline(always)]
    fn inner(&self) -> &Inner<T> {
        unsafe { &*self.ptr }
    }

    /// Shared (read-only) access via mutex
    pub fn get(&self) -> WatchGuardRef<'_, T> {
        let lock = &self.inner().state;
        lock.lock_shared();
        let val = unsafe { (&*self.inner().val.get()).assume_init_ref() };
        WatchGuardRef::new(val, lock.clone())
    }

    /// Exclusive mutable access via mutex
    pub fn get_mut(&self) -> WatchGuardMut<'_, T> {
        let lock = &self.inner().state;
        lock.lock_exclusive();
        let val = unsafe { (&mut *self.inner().val.get()).assume_init_mut() };
        WatchGuardMut::new(val, lock.clone())
    }

    /// Returns a raw pointer to the inner value
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.inner().val.get().cast::<T>()
    }

    /// Consumes the AtomicCell and returns the inner value
    pub fn into_inner(self) -> T {
        // Take ownership of the boxed inner
        let ptr = self.ptr as *mut Inner<T>;
        let boxed = unsafe { Box::from_raw(ptr) };

        // SAFETY: we own the box, no other references exist
        let value = unsafe { boxed.val.get().read().assume_init() };

        // Prevent double-drop
        mem::forget(boxed);

        value
    }
}

impl<T: Copy> AtomicCell<T> {
    /// Load value (copy) from the atomic cell
    pub fn load(&self) -> T {
        unsafe { (&*self.inner().val.get()).assume_init() }
    }
}

impl<T> AtomicCell<T> {
    /// Store a value atomically
    pub fn store(&self, val: T) {
        if mem::needs_drop::<T>() {
            // If T needs drop, swap safely
            drop(self.swap(val));
        } else {
            let mut guard = self.get_mut();
            *guard = val;
        }
    }

    /// Swap current value with new value, returning old value
    pub fn swap(&self, val: T) -> T {
        let mut guard = self.get_mut();
        mem::replace(&mut *guard, val)
    }
}

impl<T: Copy + Eq> AtomicCell<T> {
    /// Compare-and-swap: if current == stored, write new
    pub fn compare_exchange(&self, current: T, new: T) -> Result<T, WatchGuardMut<'_, T>> {
        let mut guard = self.get_mut();
        if *guard == current {
            let old = mem::replace(&mut *guard, new);
            Ok(old)
        } else {
            Err(guard)
        }
    }
}

impl<T> Clone for AtomicCell<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicCell<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            // Ensure memory ordering before destruction
            atomic::fence(Ordering::Acquire);
            unsafe { drop(Box::from_raw(self.ptr as *mut Inner<T>)) };
        }
    }
}
