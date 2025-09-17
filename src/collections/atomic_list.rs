use crate::mutex::Mutex;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::{fmt, ptr};

/// Atomic Vec operations lock free
struct AtomicInner<T> {
    /// The head of the queue.
    head: AtomicPtr<Item<T>>,

    /// The tail of the queue.
    tail: AtomicPtr<Item<T>>,

    /// A temporary tail
    t_tail: AtomicPtr<Item<T>>,

    /// Number of items in the vec
    len: AtomicUsize,

    /// Cloned reference count
    ref_count: AtomicUsize,

    /// Vec state
    state: Mutex,
}

#[repr(transparent)]
pub struct AtomicList<T> {
    ptr: *const AtomicInner<T>,
}

unsafe impl<T: Send> Send for AtomicList<T> {}
unsafe impl<T: Send> Sync for AtomicList<T> {}

impl<T> UnwindSafe for AtomicList<T> {}
impl<T> RefUnwindSafe for AtomicList<T> {}

impl<T> AtomicList<T> {
    /// Creates a new empty AtomicList
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(AtomicInner {
            head: AtomicPtr::new(null_mut()),
            tail: AtomicPtr::new(null_mut()),
            t_tail: AtomicPtr::new(null_mut()),
            len: AtomicUsize::new(0),
            ref_count: AtomicUsize::new(1),
            state: Mutex::new(),
        }));

        if ptr.is_null() {
            panic!("Invalid allocation for AtomicVec occurred");
        }

        Self { ptr }
    }

    /// Get a reference to the inner structure
    #[inline(always)]
    fn inner(&self) -> &AtomicInner<T> {
        unsafe { &*self.ptr }
    }

    /// Push a value into the list
    pub fn push(&self, val: T) {
        let item = Item::new(val);

        // If busy, try to store it in the temporary tail
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

    /// Update the real tail with a new item
    #[inline]
    fn update_tail(&self, item: *mut Item<T>) {
        let tail = self.inner().tail.load(Ordering::Acquire);
        if !tail.is_null() {
            unsafe {
                (*tail).next.store(item, Ordering::Release);
            }
        }
        self.inner().tail.store(item, Ordering::Release);

        // If the head is null, link it to the new item
        let _ = self.inner().head.compare_exchange(
            null_mut(),
            item,
            Ordering::Release,
            Ordering::Relaxed,
        );

        self.inner().len.fetch_add(1, Ordering::Relaxed);
    }

    /// Pop a value from the front of the list
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
            // Set the tail to null if head and tail are the same
            let _ =
                inner
                    .tail
                    .compare_exchange(tail, null_mut(), Ordering::Release, Ordering::Relaxed);
        }

        self.release();

        // Take ownership of the value and free the node
        let value = unsafe { ManuallyDrop::into_inner(ptr::read(&(*head).value)) };
        unsafe { drop(Box::from_raw(head)) };

        inner.len.fetch_sub(1, Ordering::Relaxed);

        Some(value)
    }

    /// Check if the list is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the current length of the list
    #[inline]
    pub fn len(&self) -> usize {
        self.inner().len.load(Ordering::Acquire)
    }

    /// Check if the internal lock is held
    #[inline]
    pub fn is_busy(&self) -> bool {
        self.inner().state.is_locked()
    }

    /// Acquire exclusive lock
    #[inline]
    pub fn lock(&self) {
        self.inner().state.lock_exclusive();
    }

    /// Release the lock and update the tail with pending items
    #[inline]
    pub fn release(&self) {
        let item = self.inner().t_tail.swap(null_mut(), Ordering::Acquire);

        if !item.is_null() {
            self.update_tail(item);
        }

        self.inner().state.unlock_exclusive();
    }
}

impl<T> AtomicList<T> {
    /// Drain all elements from the queue in O(1) critical section and return an iterator
    /// over owned elements. The internal lock is only held to detach the list; iteration
    /// happens without holding the lock.
    pub fn drain(&self) -> Option<Drain<T>> {
        // Acquire exclusive access to detach the list
        self.lock();

        // If there is a pending t_tail (queued while busy), attach it to the real tail so
        // that it becomes visible before we detach the list. This also adjusts len.
        let pending = self.inner().t_tail.swap(null_mut(), Ordering::Acquire);
        if !pending.is_null() {
            self.update_tail(pending);
        }

        // Detach the whole list atomically
        let head = self.inner().head.swap(null_mut(), Ordering::Acquire);
        self.inner().tail.store(null_mut(), Ordering::Release);
        // Reset length to 0 (we are taking ownership of all nodes)
        self.inner().len.store(0, Ordering::Relaxed);

        // Release the internal lock
        self.release();

        if head.is_null() {
            None
        } else {
            Some(Drain { current: head })
        }
    }

    /// Convert the list to a Vec by draining all elements
    /// NOTE: The previous to_vec implementation attempted to call pop() while holding the
    /// internal lock, which could deadlock. Prefer using `drain()` and collecting.
    pub fn to_vec(&self) -> Vec<T> {
        match self.drain() {
            Some(iter) => iter.collect(),
            None => Vec::new(),
        }
    }
}

/// A block in a linked list
struct Item<T> {
    /// The value
    value: ManuallyDrop<T>,

    /// The next block in the linked list
    next: AtomicPtr<Item<T>>,
}

impl<T> Item<T> {
    /// Create a new item block
    fn new<'a>(val: T) -> *mut Item<T> {
        Box::into_raw(Box::new(Item {
            value: ManuallyDrop::new(val),
            next: AtomicPtr::new(null_mut()),
        }))
    }
}

impl<T> From<Vec<T>> for AtomicList<T> {
    fn from(vec: Vec<T>) -> Self {
        let atomic_vec = Self::new();
        for v in vec {
            atomic_vec.push(v);
        }
        atomic_vec
    }
}

impl<T: Clone> From<&[T]> for AtomicList<T> {
    fn from(slice: &[T]) -> Self {
        let atomic_vec = Self::new();
        for v in slice {
            atomic_vec.push(v.clone());
        }
        atomic_vec
    }
}

impl<T> Clone for AtomicList<T> {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicList<T> {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);

            let ptr = self.ptr as *mut AtomicInner<T>;

            unsafe {
                // First: extract head and t_tail, so we don't lose pending nodes
                let head = (*ptr).head.swap(null_mut(), Ordering::Acquire);
                let pending = (*ptr).t_tail.swap(null_mut(), Ordering::Acquire);

                // 1) Free head chain
                let mut cur = head;
                while !cur.is_null() {
                    let next = (*cur).next.load(Ordering::Acquire);
                    // If the value is ManuallyDrop, perform proper drop
                    ManuallyDrop::drop(&mut (*cur).value);
                    drop(Box::from_raw(cur));
                    cur = next;
                }

                // 2) Free pending chain (t_tail) if different from head
                if !pending.is_null() && pending != head {
                    ManuallyDrop::drop(&mut (*pending).value);
                    drop(Box::from_raw(pending));
                }

                // Finally, deallocate the AtomicInner structure itself
                drop(Box::from_raw(ptr));
            }
        }
    }
}

pub struct IntoIter<T> {
    current: *mut Item<T>,
}

impl<T> IntoIterator for AtomicList<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        let head = self.inner().head.load(Ordering::Acquire);
        std::mem::forget(self);
        IntoIter { current: head }
    }
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }

        unsafe {
            let node = self.current;
            let next = (*node).next.load(Ordering::Acquire);
            self.current = next;

            let val = ManuallyDrop::into_inner(ptr::read(&(*node).value));
            drop(Box::from_raw(node));

            Some(val)
        }
    }
}

impl<T> Drop for IntoIter<T> {
    fn drop(&mut self) {
        unsafe {
            while !self.current.is_null() {
                let node = self.current;
                let next = (*node).next.load(Ordering::Acquire);
                ManuallyDrop::drop(&mut (*node).value);
                drop(Box::from_raw(node));
                self.current = next;
            }
        }
    }
}

/// Iterator returned by AtomicList::drain(); owns a detached list
pub struct Drain<T> {
    current: *mut Item<T>,
}

impl<T> Iterator for Drain<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }
        unsafe {
            let node = self.current;
            let next = (*node).next.load(Ordering::Acquire);
            self.current = next;
            let val = ManuallyDrop::into_inner(ptr::read(&(*node).value));
            drop(Box::from_raw(node));
            Some(val)
        }
    }
}

impl<T> Drop for Drain<T> {
    fn drop(&mut self) {
        unsafe {
            while !self.current.is_null() {
                let node = self.current;
                let next = (*node).next.load(Ordering::Acquire);
                ManuallyDrop::drop(&mut (*node).value);
                drop(Box::from_raw(node));
                self.current = next;
            }
        }
    }
}

pub struct Iter<'a, T> {
    current: *mut Item<T>,
    marker: PhantomData<&'a T>,
}

impl<T> AtomicList<T> {
    /// Create a non-owning iterator over references
    pub fn iter(&self) -> Iter<'_, T> {
        let head = self.inner().head.load(Ordering::Acquire);
        Iter {
            current: head,
            marker: PhantomData,
        }
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }

        unsafe {
            let node = &*self.current;
            self.current = node.next.load(Ordering::Acquire);
            Some(&*node.value)
        }
    }
}

impl<T> fmt::Debug for AtomicList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicList")
            .field("type", &std::any::type_name::<T>())
            .field("len", &self.len())
            .finish()
    }
}
