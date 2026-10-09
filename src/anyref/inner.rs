use crossync::sync::RwLock;
use std::any::Any;
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
    // Public weak-reference count, independent of the implicit allocation owner.
    pub(crate) explicit_weak: AtomicUsize,
    // Updated while holding the data lock, so metadata follows replacements.
    pub(crate) type_name: std::sync::RwLock<&'static str>,
    // actual data on heap - placed last since it's the field that gets dropped
    // while weak refs may still access the atomics above
    pub(crate) data: RwLock<Box<dyn Any>>,
}

impl AnyRefInner {
    /// Constructs a new `AnyRefInner` from a concrete value.
    /// Internally wraps the value in a `Box<dyn Any>`.
    pub(crate) fn new<T>(value: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self::from_box(Box::new(value))
    }

    pub(crate) fn from_box<T>(src: Box<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            explicit_weak: AtomicUsize::new(0),
            type_name: std::sync::RwLock::new(std::any::type_name::<T>()),
            data: RwLock::new(src as Box<dyn Any>),
        }
    }
}

impl Default for AnyRefInner {
    fn default() -> Self {
        Self::from_box(Box::new(()))
    }
}
