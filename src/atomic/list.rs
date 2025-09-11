use crate::mutex::Mutex;
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Node of the linked list
struct Item<T> {
    value: ManuallyDrop<T>, // prevents automatic drop, manual memory management
    next: *mut Item<T>,
}

impl<T> Item<T> {
    /// Allocates a new item on the heap
    fn new(val: T) -> *mut Item<T> {
        Box::into_raw(Box::new(Item {
            value: ManuallyDrop::new(val),
            next: null_mut(),
        }))
    }
}

/// Inner representation of the atomic list
struct AtomicInner<T> {
    head: CachePadded<UnsafeCell<*mut Item<T>>>,
    tail: CachePadded<UnsafeCell<*mut Item<T>>>,
    len: CachePadded<UnsafeCell<usize>>,
    ref_count: CachePadded<AtomicUsize>,
    mutex: Mutex, // protects list operations
}

/// Thread-safe atomic linked list
#[repr(transparent)]
pub struct AtomicList<T> {
    ptr: CachePadded<*const AtomicInner<T>>,
}

// Safety: UnsafeCell is protected by the mutex
unsafe impl<T: Send> Send for AtomicList<T> {}
unsafe impl<T: Send> Sync for AtomicList<T> {}

impl<T> std::panic::UnwindSafe for AtomicList<T> {}
impl<T> std::panic::RefUnwindSafe for AtomicList<T> {}

impl<T> AtomicList<T> {
    /// Creates a new empty list
    pub fn new() -> Self {
        let inner = Box::new(AtomicInner {
            head: CachePadded::new(UnsafeCell::new(null_mut())),
            tail: CachePadded::new(UnsafeCell::new(null_mut())),
            len: CachePadded::new(UnsafeCell::new(0)),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
            mutex: Mutex::new(),
        });
        let ptr = Box::into_raw(inner);
        AtomicList { ptr: CachePadded::new(ptr) }
    }

    #[inline(always)]
    fn inner(&self) -> &AtomicInner<T> {
        unsafe { &**self.ptr }
    }

    /// Pushes a new value at the end of the list
    pub fn push(&self, val: T) {
        let item = Item::new(val);
        let inner = self.inner();
        inner.mutex.lock_exclusive();
        unsafe {
            let tail = *inner.tail.get();
            if !tail.is_null() {
                (*tail).next = item;
            } else {
                *inner.head.get() = item;
            }
            *inner.tail.get() = item;
            *inner.len.get() += 1;
        }
        inner.mutex.unlock_exclusive();
    }

    /// Pops a value from the front of the list
    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();
        let head = unsafe { *inner.head.get() };
        if head.is_null() {
            inner.mutex.unlock_exclusive();
            return None;
        }
        unsafe {
            let next = (*head).next;
            *inner.head.get() = next;
            if head == *inner.tail.get() {
                *inner.tail.get() = null_mut();
            }
            *inner.len.get() -= 1;
        }
        inner.mutex.unlock_exclusive();
        unsafe {
            let value = ManuallyDrop::into_inner(ptr::read(&(*head).value));
            drop(Box::from_raw(head));
            Some(value)
        }
    }

    /// Returns the current length of the list
    pub fn len(&self) -> usize {
        let inner = self.inner();
        inner.mutex.lock_shared();
        let len = unsafe { *inner.len.get() };
        inner.mutex.unlock_shared();
        len
    }

    /// Checks if the list is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drains the list and returns an iterator
    pub fn drain(&self) -> Option<Drain<T>> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();
        let head = unsafe { *inner.head.get() };
        unsafe {
            *inner.head.get() = null_mut();
            *inner.tail.get() = null_mut();
            *inner.len.get() = 0;
        }
        inner.mutex.unlock_exclusive();
        if head.is_null() {
            None
        } else {
            Some(Drain { current: head })
        }
    }

    /// Converts the list into a Vec<T> by draining all elements
    pub fn to_vec(&self) -> Vec<T> {
        match self.drain() {
            Some(iter) => iter.collect(),
            None => Vec::new(),
        }
    }
}

/// Iterator over drained list items
pub struct Drain<T> {
    current: *mut Item<T>,
}

impl<T> Iterator for Drain<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        if self.current.is_null() {
            return None;
        }
        unsafe {
            let node = self.current;
            self.current = (*node).next;
            let val = ManuallyDrop::into_inner(ptr::read(&(*node).value));
            drop(Box::from_raw(node));
            Some(val)
        }
    }
}

impl<T> Drop for Drain<T> {
    fn drop(&mut self) {
        while !self.current.is_null() {
            unsafe {
                let node = self.current;
                self.current = (*node).next;
                ManuallyDrop::drop(&mut (*node).value);
                drop(Box::from_raw(node));
            }
        }
    }
}

impl<T> Clone for AtomicList<T> {
    fn clone(&self) -> Self {
        let inner = self.inner();
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        AtomicList { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicList<T> {
    fn drop(&mut self) {
        let inner = self.inner();
        if inner.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            unsafe {
                // Deallocate all nodes
                let boxed: Box<AtomicInner<T>> = Box::from_raw(*self.ptr as *mut AtomicInner<T>);
                let mut cur = *boxed.head.get();
                while !cur.is_null() {
                    let next = (*cur).next;
                    ManuallyDrop::drop(&mut (*cur).value);
                    drop(Box::from_raw(cur));
                    cur = next;
                }
            }
        }
    }
}

impl<T> fmt::Debug for AtomicList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicList")
            .field("len", &self.len())
            .field("type", &std::any::type_name::<T>())
            .finish()
    }
}
