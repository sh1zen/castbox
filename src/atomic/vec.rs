use crate::mutex::Mutex;
use crossbeam_utils::CachePadded;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Default initial capacity
const DEFAULT_CAP: usize = 16;

/// Thread-safe Vec, clonable
#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: *const InnerVec<T>,
}

struct InnerVec<T> {
    buf: AtomicPtr<MaybeUninit<T>>,
    read: CachePadded<AtomicUsize>,
    write: CachePadded<AtomicUsize>,
    actual_cap: AtomicUsize,
    mutex: Mutex,
    ref_count: AtomicUsize,
}

// Safety
unsafe impl<T> Send for AtomicVec<T> {}
unsafe impl<T> Sync for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAP)
    }

    pub fn with_capacity(cap: usize) -> Self {
        let inner = Box::new(InnerVec::new(cap.max(1)));
        let ptr = Box::into_raw(inner);
        Self { ptr }
    }

    #[inline(always)]
    fn inner(&self) -> &InnerVec<T> {
        unsafe { &*self.ptr }
    }

    pub fn len(&self) -> usize {
        let inner = self.inner();
        let r = inner.read.load(Ordering::Acquire);
        let w = inner.write.load(Ordering::Acquire);
        let cap = inner.actual_cap.load(Ordering::Acquire);
        (w + cap - r) % cap
    }

    pub fn is_empty(&self) -> bool {
        let inner = self.inner();
        inner.read.load(Ordering::Acquire) == inner.write.load(Ordering::Acquire)
    }

    pub fn push(&self, value: T) {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let cap = inner.actual_cap.load(Ordering::Acquire);
        let r = inner.read.load(Ordering::Acquire);
        let w = inner.write.load(Ordering::Acquire);
        let next_w = (w + 1) % cap;

        if next_w == r {
            // buffer full, need a resize
            inner.resize(cap * 2);
        }

        let cap = inner.actual_cap.load(Ordering::Acquire);
        let w = inner.write.load(Ordering::Acquire);
        let buf_ptr = inner.buf.load(Ordering::Acquire);

        unsafe {
            buf_ptr.add(w).write(MaybeUninit::new(value));
        }

        inner.write.store((w + 1) % cap, Ordering::Release);
        inner.mutex.unlock_exclusive();
    }

    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let r = inner.read.load(Ordering::Acquire);
        let w = inner.write.load(Ordering::Acquire);

        if r == w {
            inner.mutex.unlock_exclusive();
            return None;
        }

        let cap = inner.actual_cap.load(Ordering::Acquire);
        let buf_ptr = inner.buf.load(Ordering::Acquire);

        let value = unsafe { buf_ptr.add(r).read().assume_init() };
        inner.read.store((r + 1) % cap, Ordering::Release);

        inner.mutex.unlock_exclusive();
        Some(value)
    }

    pub fn get(&self, index: usize) -> Option<T>
    where
        T: Copy,
    {
        let inner = self.inner();
        if index >= self.len() {
            return None;
        }

        inner.mutex.lock_shared();
        let cap = inner.actual_cap.load(Ordering::Acquire);
        let r = inner.read.load(Ordering::Acquire);
        let buf_ptr = inner.buf.load(Ordering::Acquire);

        let value = unsafe { buf_ptr.add((r + index) % cap).read().assume_init() };
        inner.mutex.unlock_shared();
        Some(value)
    }

    pub fn as_slice(&self) -> Vec<T>
    where
        T: Copy,
    {
        let inner = self.inner();
        inner.mutex.lock_shared();

        let r = inner.read.load(Ordering::Acquire);
        let w = inner.write.load(Ordering::Acquire);
        let cap = inner.actual_cap.load(Ordering::Acquire);
        let len = (w + cap - r) % cap;

        let buf_ptr = inner.buf.load(Ordering::Acquire);
        let mut v = Vec::with_capacity(len);

        if r < w {
            for i in r..w {
                let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                v.push(*item);
            }
        } else {
            // wrap-around
            for i in r..cap {
                let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                v.push(*item);
            }
            for i in 0..w {
                let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                v.push(*item);
            }
        }

        inner.mutex.unlock_shared();
        v
    }
}

impl<T> InnerVec<T> {
    fn new(cap: usize) -> Self {
        Self {
            buf: AtomicPtr::new(Self::alloc_buffer(cap)),
            read: CachePadded::new(AtomicUsize::new(0)),
            write: CachePadded::new(AtomicUsize::new(0)),
            actual_cap: AtomicUsize::new(cap),
            mutex: Mutex::new(),
            ref_count: AtomicUsize::new(1),
        }
    }

    fn resize(&self, new_cap: usize) {
        let old_cap = self.actual_cap.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let w = self.write.load(Ordering::Acquire);

        let old_buf = self.buf.load(Ordering::Acquire);
        let new_buf = Self::alloc_buffer(new_cap);

        let len = (w + old_cap - r) % old_cap;

        unsafe {
            if r < w {
                std::ptr::copy_nonoverlapping(old_buf.add(r), new_buf, len);
            } else if len > 0 {
                let first = old_cap - r;
                std::ptr::copy_nonoverlapping(old_buf.add(r), new_buf, first);
                std::ptr::copy_nonoverlapping(old_buf, new_buf.add(first), w);
            }

            let old_layout = std::alloc::Layout::array::<MaybeUninit<T>>(old_cap).unwrap();
            std::alloc::dealloc(old_buf.cast::<u8>(), old_layout);
        }

        self.buf.store(new_buf, Ordering::Release);
        self.read.store(0, Ordering::Release);
        self.write.store(len, Ordering::Release);
        self.actual_cap.store(new_cap, Ordering::Release);
    }

    fn alloc_buffer(cap: usize) -> *mut MaybeUninit<T> {
        let layout = std::alloc::Layout::array::<MaybeUninit<T>>(cap).unwrap();
        unsafe {
            let ptr = std::alloc::alloc(layout) as *mut MaybeUninit<T>;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            ptr
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicVec")
            .field("type", &std::any::type_name::<T>())
            .field("len", &self.len())
            .finish()
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
        // decrement the reference counter and check if we were the last reference
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {

            // we were the last reference: take ownership and destroy everything
            unsafe {
                // recover the Box to safely dismantle it (avoids use-after-free)
                let boxed: Box<InnerVec<T>> = Box::from_raw(self.ptr as *mut InnerVec<T>);

                let r = boxed.read.load(Ordering::Acquire);
                let w = boxed.write.load(Ordering::Acquire);
                let cap = boxed.actual_cap.load(Ordering::Acquire);
                let buf_ptr = boxed.buf.load(Ordering::Acquire);

                // number of actual elements (in [0, cap-1])
                let len = (w + cap - r) % cap;

                // if T requires Drop, call drop_in_place only on valid elements
                if std::mem::needs_drop::<T>() && len > 0 {
                    for i in 0..len {
                        let idx = (r + i) % cap;
                        std::ptr::drop_in_place(buf_ptr.add(idx).cast::<T>());
                    }
                }

                // deallocate the raw buffer
                let layout = std::alloc::Layout::array::<MaybeUninit<T>>(cap).unwrap();
                std::alloc::dealloc(buf_ptr.cast::<u8>(), layout);
            }
        }
    }
}

impl<T> FromIterator<T> for AtomicVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let v = AtomicVec::new();
        for item in iter {
            v.push(item);
        }
        v
    }
}
