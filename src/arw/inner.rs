use crossync::sync::RwLock;
use std::any::Any;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::sync::atomic::AtomicUsize;

/// Max number of references that an Arw could have
pub(crate) const MAX_REFCOUNT: usize = isize::MAX as usize;

/// The inner structure that holds the actual data
pub(crate) struct ArwInner<T: Sized> {
    pub(crate) strong: AtomicUsize,
    pub(crate) weak: AtomicUsize,
    marker: PhantomData<T>,
    /// The value is wrapped in ManuallyDrop so we can control when T is dropped
    /// separately from when RwLock is dropped.
    /// This allows try_unwrap to extract T without double-dropping.
    pub(crate) val: RwLock<ManuallyDrop<T>>,
}

impl<T> ArwInner<T> {
    /// Constructs a new `ArwInner` from a concrete value.
    pub(crate) fn new(val: T) -> Self
    where
        T: Any,
    {
        Self {
            val: RwLock::new(ManuallyDrop::new(val)),
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            marker: PhantomData,
        }
    }
}

impl<T: Default> Default for ArwInner<T> {
    fn default() -> Self {
        Self {
            val: RwLock::new(ManuallyDrop::new(Default::default())),
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            marker: PhantomData,
        }
    }
}
