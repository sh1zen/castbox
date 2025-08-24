use crate::mutex::Backoff;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::{fmt, ptr};

const AVAILABLE: bool = true;
const UPDATING: bool = false;

/// Atomic Vec operations lock free
struct AtomicInner<T> {
    /// The head of the queue.
    head: AtomicPtr<Item<T>>,

    /// The tail of the queue.
    tail: AtomicPtr<Item<T>>,

    /// a temp tail
    t_tail: AtomicPtr<Item<T>>,

    /// numbers of items in the vec
    len: AtomicUsize,

    /// vec state
    state: AtomicBool,

    /// cloned ref
    ref_count: AtomicUsize,
}

#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: *const AtomicInner<T>,
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
            t_tail: AtomicPtr::new(null_mut()),
            len: AtomicUsize::new(0),
            state: AtomicBool::new(AVAILABLE),
            ref_count: AtomicUsize::new(1),
        }));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for AtomicVec");
        }
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicInner<T> {
        unsafe { &*self.ptr }
    }

    pub fn push(&self, val: T) {
        let item = Item::new(val);

        if self.is_busy() {
            if self
                .inner()
                .t_tail
                .compare_exchange(null_mut(), item, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }

        self.lock();
        self.update_tail(item);
        self.release();
    }

    #[inline]
    fn update_tail(&self, item: *mut Item<T>) {
        let tail = self.inner().tail.load(Ordering::Acquire);
        if !tail.is_null() {
            unsafe {
                (*tail).next.store(item, Ordering::Release);
            }
        }
        self.inner().tail.store(item, Ordering::Release);

        // if the head is pointing to null we need to link it.
        let _ = self.inner().head.compare_exchange(
            null_mut(),
            item,
            Ordering::Release,
            Ordering::Relaxed,
        );

        self.inner().len.fetch_add(1, Ordering::Relaxed);
    }

    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();

        self.lock();

        let head = inner.head.load(Ordering::Acquire);

        if head.is_null() {
            self.release();
            return None;
        }

        let next_block = unsafe { (&*head).next.load(Ordering::Acquire) };
        inner.head.store(next_block, Ordering::Release);

        let tail = inner.tail.load(Ordering::Acquire);
        if head == tail {
            // set the tail to nullptr if tail and head are pointing to the same block
            let _ =
                inner
                    .tail
                    .compare_exchange(tail, null_mut(), Ordering::Release, Ordering::Relaxed);
        }

        self.release();

        let value = unsafe { ManuallyDrop::into_inner(ptr::read(&(*head).value)) };
        unsafe { drop(Box::from_raw(head)) };

        inner.len.fetch_sub(1, Ordering::Relaxed);

        Some(value)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_busy(&self) -> bool {
        self.inner().state.load(Ordering::Acquire) != AVAILABLE
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
        let item = self.inner().t_tail.swap(null_mut(), Ordering::Acquire);

        if !item.is_null() {
            self.update_tail(item);
        }

        self.inner().state.store(AVAILABLE, Ordering::Release);
    }
}

/// A block in a linked list.
struct Item<T> {
    /// The value.
    value: ManuallyDrop<T>,

    /// The next block in the linked list.
    next: AtomicPtr<Item<T>>,
}

impl<T> Item<T> {
    fn new<'a>(val: T) -> *mut Item<T> {
        Box::into_raw(Box::new(Item {
            value: ManuallyDrop::new(val),
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

impl<T> Clone for AtomicVec<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);

            let ptr = self.ptr as *mut AtomicInner<T>;

            unsafe {
                let mut head = (*ptr).head.load(Ordering::Acquire);
                loop {
                    if head.is_null() {
                        break;
                    }
                    let next = (*head).next.load(Ordering::Acquire);
                    ManuallyDrop::drop(&mut (*head).value);
                    drop(Box::from_raw(head));
                    head = next;
                }
            }

            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl<T> fmt::Debug for AtomicVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicVec")
            .field("type", &std::any::type_name::<T>())
            .field("len", &self.len())
            .finish()
    }
}
