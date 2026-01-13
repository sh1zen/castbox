use crossync::sync::RwLock;
use std::any::{Any, TypeId};
use std::sync::atomic::AtomicUsize;

/// Max number of references that an AnyRef could have
pub(crate) const MAX_REFCOUNT: usize = isize::MAX as usize;

/// The main worker of AnyRef.
///
/// # Memory Layout Note
/// The `strong` and `weak` atomic counters are placed first to ensure they can be
/// safely accessed via raw pointers even when the `data` field is being dropped.
/// This is crucial for the weak reference upgrade path which needs to check
/// the strong count while the data might be concurrently dropped.
#[repr(C)]
pub(crate) struct AnyRefInner {
    // strong ref count - placed first for optimal access pattern
    pub(crate) strong: AtomicUsize,
    // weak ref count
    pub(crate) weak: AtomicUsize,
    // data type id
    pub(crate) type_id: TypeId,
    // type name
    pub(crate) type_name: &'static str,
    // actual data on heap - placed last since it's the field that gets dropped
    // while weak refs may still access the atomics above
    pub(crate) data: RwLock<Box<dyn Any>>,
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
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            data: RwLock::new(src as Box<dyn Any>),
        }
    }
}

impl Default for AnyRefInner {
    fn default() -> Self {
        Self::from_box(Box::new(()))
    }
}
