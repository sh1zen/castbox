use crossync::sync::RwLock;
use std::any::Any;
use std::marker::PhantomData;
use std::sync::atomic::AtomicUsize;

/// Max number of references that an Arw could have
pub(crate) const MAX_REFCOUNT: usize = isize::MAX as usize;

/// The inner structure that holds the actual data
pub(crate) struct ArwInner<T: Sized> {
    pub(crate) strong: AtomicUsize,
    pub(crate) weak: AtomicUsize,
    marker: PhantomData<T>,
    pub(crate) val: RwLock<T>,
}

impl<T> ArwInner<T> {
    /// Constructs a new `ArwInner` from a concrete value.
    pub(crate) fn new(val: T) -> Self
    where
        T: Any,
    {
        Self {
            val: RwLock::new(val),
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            marker: PhantomData,
        }
    }
}

impl<T: Default> Default for ArwInner<T> {
    fn default() -> Self {
        Self {
            val: RwLock::new(Default::default()),
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            marker: PhantomData,
        }
    }
}
