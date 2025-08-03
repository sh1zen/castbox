use crossync::sync::Mutex;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::sync::atomic::AtomicUsize;

/// Max number of reference that an anyref could have
pub(crate) const MAX_REFCOUNT: usize = isize::MAX as usize;

/// Actually the main worker of AnyRef
pub(crate) struct AnyRefInner {
    // strong ref count
    pub(crate) strong: AtomicUsize,
    // weak ref count
    pub(crate) weak: AtomicUsize,
    // data type id
    pub(crate) type_id: TypeId,
    // actual data on heap
    pub(crate) data: UnsafeCell<Box<dyn Any>>,
    // type name
    pub(crate) type_name: &'static str,
    // syncing mechanism for ref access to data
    pub(crate) lock: Mutex,
}

impl AnyRefInner {
    /// Constructs a new `AnyRefInner` from a concrete value.
    /// Internally wraps the value in a `Box<dyn Any>`.
    pub(crate) fn new<T>(value: T) -> Self
    where
        T: Any + Sized,
    {
        Self::from_box(Box::new(value))
    }

    pub(crate) fn from_box<T>(src: Box<T>) -> Self
    where
        T: Any + Sized,
    {
        Self {
            data: UnsafeCell::new(src as Box<dyn Any>),
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            lock: Mutex::new(),
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
        }
    }

    #[inline(always)]
    fn internal_get(&self) -> *mut dyn Any {
        let ptr = self.data.get();
        let data = unsafe { &mut **ptr as *mut dyn Any };
        data
    }

    pub(crate) fn get_ref(&self) -> &dyn Any {
        unsafe { &*self.internal_get() }
    }

    pub(crate) fn get_mut_ref(&self) -> &mut dyn Any {
        unsafe { &mut *self.internal_get() }
    }
}

impl Default for AnyRefInner {
    fn default() -> Self {
        Self::from_box(Box::new(()))
    }
}
