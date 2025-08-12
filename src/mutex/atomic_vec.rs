use crossbeam_queue::SegQueue;
use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub struct AtomicVec<T> {
    /// The head of the queue.
    head: Block<T>,

    /// The tail of the queue.
    tail: Block<T>,

    len: AtomicUsize,

    /// Indicates that dropping a `AtomicVec<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for AtomicVec<T> {}
unsafe impl<T: Send> Sync for AtomicVec<T> {}

impl<T> UnwindSafe for AtomicVec<T> {}
impl<T> RefUnwindSafe for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        AtomicVec {
            head: Block::empty(),
            tail: Block::empty(),
            len: AtomicUsize::new(0),
            _marker: PhantomData,
        }
    }

   pub fn push(&self, val: T) {
       let block = Block::new(val);

       let head = self.head.next.load(Ordering::Relaxed);

   }
}

/// A block in a linked list.
///
/// Each block in the list can hold up to `BLOCK_CAP` values.
struct Block<T> {
    /// The value.
    value: UnsafeCell<MaybeUninit<T>>,

    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,

    /// The prev block in the linked list.
    prev: AtomicPtr<Block<T>>,

    //state: BlockState,
}

impl<T> Block<T> {

    fn new(val: T) -> Self {
        Block {
            value: UnsafeCell::new(MaybeUninit::new(val)),
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn empty() -> Self {
        Block {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
        }
    }
}