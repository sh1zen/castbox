use crate::mutex::backoff::Backoff;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use std::{fmt, ptr};

const AVAILABLE: u8 = 0;
const UPDATING: u8 = 1;

/// Atomic Vec operations lock free
struct AtomicInner<T> {
    /// The head of the queue.
    head: AtomicPtr<Block<T>>,

    /// The tail of the queue.
    tail: AtomicPtr<Block<T>>,

    len: AtomicUsize,

    state: AtomicU8,

    /// Indicates that dropping a `AtomicVec<T>` may drop values of type `T`.
    _marker: PhantomData<T>,

    /// cloned ref
    ref_count: AtomicUsize,
}

#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: NonNull<AtomicInner<T>>,
}

unsafe impl<T: Send> Send for AtomicVec<T> {}
unsafe impl<T: Send> Sync for AtomicVec<T> {}

impl<T> UnwindSafe for AtomicVec<T> {}
impl<T> RefUnwindSafe for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(AtomicInner {
            head: AtomicPtr::new(null_mut()),
            tail: AtomicPtr::new(null_mut()),
            len: AtomicUsize::new(0),
            state: AtomicU8::new(0),
            _marker: PhantomData,
            ref_count: AtomicUsize::new(1),
        }));
        Self {
            ptr: NonNull::new(ptr).expect("Happened an invalid allocation for AtomicVec"),
        }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicInner<T> {
        unsafe { self.ptr.as_ref() }
    }

    pub fn push(&self, val: T) {
        let atomic_vec = self.inner();

        let backoff = Backoff::new();
        let block = Block::new(val);

        loop {
            self.lock();
            let tail = atomic_vec.tail.load(Ordering::Acquire);

            // update the tail
            match atomic_vec.tail.compare_exchange(
                tail,
                block,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if !tail.is_null() {
                        unsafe {
                            (*tail).next.store(block, Ordering::Release);
                        }
                    }
                    self.release();
                    break;
                }
                Err(_) => {
                    self.release();
                    backoff.snooze();
                }
            }
        }

        // if the head is pointing to null we need to link it.
        let _ = atomic_vec.head.compare_exchange(
            null_mut(),
            block,
            Ordering::Release,
            Ordering::Relaxed,
        );

        atomic_vec.len.fetch_add(1, Ordering::Release);
    }

    pub fn pop(&self) -> Option<T> {
        let atomic_vec = self.inner();

        self.lock();

        let head = atomic_vec.head.load(Ordering::Acquire);

        if head.is_null() {
            self.release();
            return None;
        }

        let tail = atomic_vec.tail.load(Ordering::Acquire);

        if head == tail {
            // set the tail to nullptr if tail and head are pointing to the same block
            atomic_vec.tail.store(null_mut(), Ordering::Release);
        }

        let next_block = unsafe { (&*head).next.load(Ordering::Acquire) };
        atomic_vec.head.store(next_block, Ordering::Release);
        self.release();

        atomic_vec.len.fetch_sub(1, Ordering::Release);

        let value = unsafe { ManuallyDrop::into_inner(ptr::read(&(*head).value)) };
        unsafe { drop(Box::from_raw(head)) };

        Some(value)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    #[inline]
    fn lock(&self) {
        let backoff = Backoff::new();
        while self
            .inner()
            .state
            .compare_exchange(AVAILABLE, UPDATING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            backoff.snooze();
        }
    }

    #[inline]
    fn release(&self) {
        self.inner().state.store(AVAILABLE, Ordering::Release);
    }
}

/// A block in a linked list.
struct Block<T> {
    /// The value.
    value: ManuallyDrop<T>,

    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,
}

impl<T> Block<T> {
    fn new<'a>(val: T) -> *mut Block<T> {
        Box::into_raw(Box::new(Block {
            value: ManuallyDrop::new(val),
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

impl<T> Clone for AtomicVec<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Acquire);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Release);

            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
}

impl<T> fmt::Debug for AtomicVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicVec")
            .field("type", &std::any::type_name::<T>())
            .field("len", &self.inner().len)
            .finish()
    }
}
