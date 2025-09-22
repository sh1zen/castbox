use crate::mutex::{Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default initial capacity
const DEFAULT_CAP: usize = 16;

/// Thread-safe Vec, clonable
#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: CachePadded<*const InnerVec<T>>,
}

struct InnerVec<T> {
    // padded fields for better cache isolation
    buf: CachePadded<UnsafeCell<*mut MaybeUninit<T>>>,
    head: CachePadded<UnsafeCell<usize>>,
    len: CachePadded<UnsafeCell<usize>>,
    cap: CachePadded<UnsafeCell<usize>>,
    // reference count for clone/drop
    ref_count: CachePadded<AtomicUsize>,

    // reader/writer mutex: lock_shared / lock_exclusive
    mutex: Mutex,
}

unsafe impl<T> Send for AtomicVec<T> {}
unsafe impl<T> Sync for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAP)
    }

    pub fn with_capacity(cap: usize) -> Self {
        let inner = Box::new(InnerVec::new(cap.max(1)));
        let ptr = Box::into_raw(inner);
        Self {
            ptr: CachePadded::new(ptr),
        }
    }

    /// init_with: costruisce e popola il buffer — operazione esclusiva
    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::with_capacity(cap);
        let inner = vec.inner();

        inner.mutex.lock_exclusive();
        let buf_ptr = unsafe { *inner.buf.get() };
        for i in 0..cap {
            unsafe {
                buf_ptr.add(i).write(MaybeUninit::new(initializer()));
            }
        }
        inner.set_head(0);
        inner.set_len(cap);
        inner.set_cap(cap);
        inner.mutex.unlock_exclusive();

        vec
    }

    #[inline(always)]
    fn inner(&self) -> &InnerVec<T> {
        unsafe { &**self.ptr }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        let inner = self.inner();
        inner.mutex.lock_shared();
        let l = inner.get_len();
        inner.mutex.unlock_shared();
        l
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        let inner = self.inner();
        inner.mutex.lock_shared();
        let c = inner.get_cap();
        inner.mutex.unlock_shared();
        c
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// push: esclusivo. Semantica: push_back (aggiunge alla fine)
    pub fn push(&self, value: T) {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let mut cap = inner.get_cap();
        let mut len = inner.get_len();
        let head = inner.get_head();

        if len == cap {
            // buffer full -> resize (manteniamo il lock esclusivo durante la resize)
            inner.resize_internal(cap * 2);
            cap = inner.get_cap();
            len = inner.get_len();
        }

        let buf_ptr = unsafe { *inner.buf.get() };

        let write_pos = if len == 0 { 0 } else { (head + len) % cap };

        unsafe {
            buf_ptr.add(write_pos).write(MaybeUninit::new(value));
        }
        inner.set_len(len + 1);

        inner.mutex.unlock_exclusive();
    }

    /// pop: esclusivo. Semantica: pop_front (rimuove il primo elemento)
    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let len = inner.get_len();
        if len == 0 {
            inner.mutex.unlock_exclusive();
            return None;
        }

        let head = inner.get_head();
        let cap = inner.get_cap();
        let buf_ptr = inner.get_buf();

        let value = unsafe { buf_ptr.add(head).read().assume_init() };

        if len == 1 {
            // now empty: reset head to 0 for canonical state
            inner.set_head(0);
            inner.set_len(0);
        } else {
            inner.set_head((head + 1) % cap);
            inner.set_len(len - 1);
        }

        inner.mutex.unlock_exclusive();
        Some(value)
    }

    /// get: shared. index è relativo al contenuto (0..len-1)
    pub fn get(&self, index: usize) -> Option<WatchGuardRef<'_, T>> {
        let inner = self.inner();
        inner.mutex.lock_shared();

        let head = inner.get_head();
        let len = inner.get_len();
        let cap = inner.get_cap();

        if index >= len {
            inner.mutex.unlock_shared();
            return None;
        }

        let pos = (head + index) % cap;
        let buf_ptr = unsafe { *inner.buf.get() };

        let value = unsafe { (&*buf_ptr.add(pos)).assume_init_ref() };

        Some(WatchGuardRef::new(value, inner.mutex.clone()))
    }

    /// resize pubblica: acquisisce lock esclusivo e chiama la funzione di resize interna
    pub fn resize(&self, new_cap: usize) -> usize {
        let inner = self.inner();
        inner.mutex.lock_exclusive();
        inner.resize_internal(new_cap);
        inner.mutex.unlock_exclusive();
        new_cap
    }

    // Simplified reset_with: no RAII guard, less panic-safety (user requested simplification)
    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> usize {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let old_head = inner.get_head();
        let old_len = inner.get_len();
        let old_cap = inner.get_cap();
        let old_buf = inner.get_buf();

        // allocate and initialize new buffer. NOTE: if initializer panics here,
        // the new buffer may leak — this function is intentionally simplified.
        let new_cap = new_cap.max(1);
        let new_buf = InnerVec::alloc_buffer(new_cap);
        for i in 0..new_cap {
            unsafe {
                new_buf.add(i).write(MaybeUninit::new(initializer()));
            }
        }

        // drop old elements safely
        if std::mem::needs_drop::<T>() && old_len > 0 {
            for i in 0..old_len {
                let idx = (old_head + i) % old_cap;
                unsafe {
                    ptr::drop_in_place(old_buf.add(idx).cast::<T>());
                }
            }
        }

        unsafe {
            let old_layout = std::alloc::Layout::array::<MaybeUninit<T>>(old_cap).unwrap();
            std::alloc::dealloc(old_buf.cast::<u8>(), old_layout);
        }

        // commit new buffer: it is fully initialized and contains `new_cap` elements
        inner.set_buf(new_buf);
        inner.set_head(0);
        inner.set_len(new_cap);
        inner.set_cap(new_cap);

        inner.mutex.unlock_exclusive();
        new_cap
    }
}

impl<T> AtomicVec<T> {
    /// as_slice: shared
    /// restituisce una copia del contenuto (shared)
    pub fn as_slice(&self) -> Vec<T>
    where
        T: Clone,
    {
        let inner = self.inner();
        inner.mutex.lock_shared();

        let head = inner.get_head();
        let len = inner.get_len();
        let cap = inner.get_cap();

        let buf_ptr = inner.get_buf();
        let mut v = Vec::with_capacity(len);

        if len > 0 {
            for i in 0..len {
                let pos = (head + i) % cap;
                let item = unsafe { (&*buf_ptr.add(pos)).assume_init_ref() };
                v.push(item.clone());
            }
        }

        inner.mutex.unlock_shared();
        v
    }

    /// Consuma i dati correnti e li restituisce come `Vec<T>`.
    /// Dopo la chiamata, l'AtomicVec risulta vuoto.
    pub fn as_vec(&self) -> Vec<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let head = inner.get_head();
        let len = inner.get_len();
        let cap = inner.get_cap();

        let buf_ptr = inner.get_buf();
        let mut out = Vec::with_capacity(len);

        unsafe {
            for i in 0..len {
                let pos = (head + i) % cap;
                out.push(buf_ptr.add(pos).read().assume_init());
            }
        }

        // reset indices: buffer ora considerato vuoto
        inner.set_head(0);
        inner.set_len(0);

        inner.mutex.unlock_exclusive();
        out
    }
}

impl<T> InnerVec<T> {
    fn new(cap: usize) -> Self {
        let buf = Self::alloc_buffer(cap);
        Self {
            buf: CachePadded::new(UnsafeCell::new(buf)),
            head: CachePadded::new(UnsafeCell::new(0)),
            len: CachePadded::new(UnsafeCell::new(0)),
            cap: CachePadded::new(UnsafeCell::new(cap)),
            mutex: Mutex::new(),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        }
    }

    // getter/setter per head/len/cap/buf (usare SOLO con lock appropriato)
    #[inline(always)]
    fn get_head(&self) -> usize {
        unsafe { *self.head.get() }
    }

    #[inline(always)]
    fn set_head(&self, v: usize) {
        unsafe { *self.head.get() = v }
    }

    #[inline(always)]
    fn get_len(&self) -> usize {
        unsafe { *self.len.get() }
    }

    #[inline(always)]
    fn set_len(&self, v: usize) {
        unsafe { *self.len.get() = v }
    }

    #[inline(always)]
    fn get_cap(&self) -> usize {
        unsafe { *self.cap.get() }
    }

    #[inline(always)]
    fn set_cap(&self, v: usize) {
        unsafe { *self.cap.get() = v }
    }

    #[inline(always)]
    fn get_buf(&self) -> *mut MaybeUninit<T> {
        unsafe { *self.buf.get() }
    }
    #[inline(always)]
    fn set_buf(&self, p: *mut MaybeUninit<T>) {
        unsafe { *self.buf.get() = p }
    }

    /// internal resize: assume di avere il lock esclusivo
    fn resize_internal(&self, new_cap: usize) {
        let old_cap = self.get_cap();

        // todo add support to shrinking
        if old_cap >= new_cap {
            return;
        }

        let head = self.get_head();
        let len = self.get_len();
        let old_buf = self.get_buf();
        let new_buf = Self::alloc_buffer(new_cap);

        unsafe {
            if len > 0 {
                for i in 0..len {
                    let idx = (head + i) % old_cap;
                    let value = old_buf.add(idx).read().assume_init();
                    new_buf.add(i).write(MaybeUninit::new(value));
                }
            }

            // dealloc old buffer
            let old_layout = std::alloc::Layout::array::<MaybeUninit<T>>(old_cap).unwrap();
            std::alloc::dealloc(old_buf.cast::<u8>(), old_layout);
        }

        // aggiorniamo pointer e indici
        self.set_buf(new_buf);
        self.set_head(0);
        self.set_len(len);
        self.set_cap(new_cap);
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
        // increment refcount
        let inner = self.inner();
        inner.ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        let inner = self.inner();
        if inner.ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);

            unsafe {
                let boxed: Box<InnerVec<T>> = Box::from_raw(*self.ptr as *mut InnerVec<T>);

                let head = boxed.get_head();
                let len = boxed.get_len();
                let cap = boxed.get_cap();
                let buf_ptr = boxed.get_buf();

                if std::mem::needs_drop::<T>() && len > 0 {
                    for i in 0..len {
                        let idx = (head + i) % cap;
                        ptr::drop_in_place(buf_ptr.add(idx).cast::<T>());
                    }
                }

                let layout = std::alloc::Layout::array::<MaybeUninit<T>>(cap).unwrap();
                std::alloc::dealloc(buf_ptr.cast::<u8>(), layout);
                // boxed is dropped here
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
