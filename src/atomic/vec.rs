use crate::mutex::Mutex;
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
    // buffer pointer (interior mutability: modificato solo sotto lock_exclusive,
    // letto sotto lock_shared)
    buf: UnsafeCell<*mut MaybeUninit<T>>,

    // indices and capacity (interior mutability)
    read: CachePadded<UnsafeCell<usize>>,
    write: CachePadded<UnsafeCell<usize>>,
    actual_cap: CachePadded<UnsafeCell<usize>>,

    // reader/writer mutex: lock_shared / lock_exclusive
    mutex: Mutex,

    // reference count for clone/drop; padded to avoid false sharing
    ref_count: CachePadded<AtomicUsize>,
}

// We assert Send+Sync on AtomicVec: InnerVec contains UnsafeCell but all accesses
// are synchronized with `mutex`. The user must ensure Mutex actually synchronizes.
unsafe impl<T> Send for AtomicVec<T> {}
unsafe impl<T> Sync for AtomicVec<T> {}

impl<T> AtomicVec<T> {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAP)
    }

    pub fn with_capacity(cap: usize) -> Self {
        let inner = Box::new(InnerVec::new(cap.max(2)));
        let ptr = Box::into_raw(inner);
        Self {
            ptr: CachePadded::new(ptr),
        }
    }

    /// init_with: costruisce e popola il buffer — operazione esclusiva
    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::with_capacity(cap);
        let inner = vec.inner();

        // esclusivo perché modifichiamo il buffer e gli indici
        inner.mutex.lock_exclusive();
        let buf_ptr = unsafe { *inner.buf.get() };
        for i in 0..cap {
            unsafe {
                buf_ptr.add(i).write(MaybeUninit::new(initializer()));
            }
        }
        // write/read: dopo popolazione - consideriamo il buffer "pieno"
        inner.set_read(0);
        inner.set_write(cap % inner.get_cap()); // se cap==actual_cap allora cap%cap == 0, ma semanticamente va bene
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
        let r = inner.get_read();
        let w = inner.get_write();
        let cap = inner.get_cap();
        let len = (w + cap - r) % cap;
        inner.mutex.unlock_shared();
        len
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

    /// push: esclusivo
    pub fn push(&self, value: T) {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let cap = inner.get_cap();
        let r = inner.get_read();
        let w = inner.get_write();
        let next_w = (w + 1) % cap;

        if next_w == r {
            // buffer full -> resize (manteniamo il lock esclusivo durante la resize)
            inner.resize_internal(cap * 2);
        }

        // r/w/cap possono essere cambiati dalla resize: rileggiamo gli indici
        let cap = inner.get_cap();
        let w = inner.get_write();
        let buf_ptr = unsafe { *inner.buf.get() };

        unsafe {
            buf_ptr.add(w).write(MaybeUninit::new(value));
        }

        inner.set_write((w + 1) % cap);
        inner.mutex.unlock_exclusive();
    }

    /// pop: esclusivo
    pub fn pop(&self) -> Option<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let r = inner.get_read();
        let w = inner.get_write();

        if r == w {
            inner.mutex.unlock_exclusive();
            return None;
        }

        let cap = inner.get_cap();
        let buf_ptr = unsafe { *inner.buf.get() };

        let value = unsafe { buf_ptr.add(r).read().assume_init() };
        inner.set_read((r + 1) % cap);

        inner.mutex.unlock_exclusive();
        Some(value)
    }

    /// get: shared. index è relativo al contenuto (0..len-1)
    pub fn get(&self, index: usize) -> Option<T>
    where
        T: Copy,
    {
        let inner = self.inner();
        inner.mutex.lock_shared();

        let r = inner.get_read();
        let cap = inner.get_cap();

        if index >= cap {
            inner.mutex.unlock_shared();
            return None;
        }

        let pos = (r + index) % cap;
        let buf_ptr = unsafe { *inner.buf.get() };

        let value = unsafe { (&*buf_ptr.add(pos)).assume_init_ref() };
        let ret = *value;

        inner.mutex.unlock_shared();
        Some(ret)
    }

    /// resize pubblica: acquisisce lock esclusivo e chiama la funzione di resize interna
    pub fn resize(&self, new_len: usize) {
        let inner = self.inner();
        inner.mutex.lock_exclusive();
        inner.resize_internal(new_len);
        inner.mutex.unlock_exclusive();
    }
}

impl<T> AtomicVec<T> {
    /// as_slice: shared
    /// as_slice: restituisce una copia del contenuto (shared)
    pub fn as_slice(&self) -> Vec<T>
    where
        T: Clone,
    {
        let inner = self.inner();
        inner.mutex.lock_shared();

        let r = inner.get_read();
        let w = inner.get_write();
        let cap = inner.get_cap();
        let len = (w + cap - r) % cap;

        let buf_ptr = inner.get_buf();
        let mut v = Vec::with_capacity(len);

        if len > 0 {
            if r < w {
                for i in r..w {
                    let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                    v.push(item.clone());
                }
            } else {
                for i in r..cap {
                    let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                    v.push(item.clone());
                }
                for i in 0..w {
                    let item = unsafe { (&*buf_ptr.add(i)).assume_init_ref() };
                    v.push(item.clone());
                }
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

        let r = inner.get_read();
        let w = inner.get_write();
        let cap = inner.get_cap();
        let len = (w + cap - r) % cap;

        let buf_ptr = inner.get_buf();
        let mut out = Vec::with_capacity(len);

        unsafe {
            if len > 0 {
                if r < w {
                    for i in r..w {
                        out.push(buf_ptr.add(i).read().assume_init());
                    }
                } else {
                    for i in r..cap {
                        out.push(buf_ptr.add(i).read().assume_init());
                    }
                    for i in 0..w {
                        out.push(buf_ptr.add(i).read().assume_init());
                    }
                }
            }
        }

        // reset indices: buffer ora considerato vuoto
        inner.set_read(0);
        inner.set_write(0);

        inner.mutex.unlock_exclusive();
        out
    }
}

impl<T> InnerVec<T> {
    fn new(cap: usize) -> Self {
        let buf = Self::alloc_buffer(cap);
        Self {
            buf: UnsafeCell::new(buf),
            read: CachePadded::new(UnsafeCell::new(0)),
            write: CachePadded::new(UnsafeCell::new(0)),
            actual_cap: CachePadded::new(UnsafeCell::new(cap)),
            mutex: Mutex::new(),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        }
    }

    // getter/setter per read/write/cap (usare SOLO con lock appropriato)
    #[inline(always)]
    fn get_read(&self) -> usize {
        unsafe { *self.read.get() }
    }
    #[inline(always)]
    fn set_read(&self, v: usize) {
        unsafe { *self.read.get() = v }
    }
    #[inline(always)]
    fn get_write(&self) -> usize {
        unsafe { *self.write.get() }
    }
    #[inline(always)]
    fn set_write(&self, v: usize) {
        unsafe { *self.write.get() = v }
    }
    #[inline(always)]
    fn get_cap(&self) -> usize {
        unsafe { *self.actual_cap.get() }
    }
    #[inline(always)]
    fn set_cap(&self, v: usize) {
        unsafe { *self.actual_cap.get() = v }
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
        if old_cap >= new_cap {
            return;
        }

        let r = self.get_read();
        let w = self.get_write();

        let old_buf = self.get_buf();
        let new_buf = Self::alloc_buffer(new_cap);

        // numero di elementi validi
        let len = (w + old_cap - r) % old_cap;

        unsafe {
            if len == 0 {
                // non c'è nulla da copiare
            } else if r < w {
                // blocco contiguo
                ptr::copy_nonoverlapping(old_buf.add(r), new_buf, len);
            } else {
                // wrap-around: copia in due blocchi
                let first = old_cap - r;
                ptr::copy_nonoverlapping(old_buf.add(r), new_buf, first);
                ptr::copy_nonoverlapping(old_buf, new_buf.add(first), w);
            }

            // dealloc old buffer
            let old_layout = std::alloc::Layout::array::<MaybeUninit<T>>(old_cap).unwrap();
            std::alloc::dealloc(old_buf.cast::<u8>(), old_layout);
        }

        // aggiorniamo pointer e indici
        self.set_buf(new_buf);
        self.set_read(0);
        self.set_write(len);
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
        // aggiornamento ref_count (atomic)
        self.inner().ref_count.fetch_add(1, Ordering::Relaxed);
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for AtomicVec<T> {
    fn drop(&mut self) {
        // decrement refcount; se arriviamo a zero distruggiamo
        if self.inner().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            // sincronizza con altre operazioni in corso (se ce ne fossero)
            std::sync::atomic::fence(Ordering::Acquire);

            unsafe {
                // riprendiamo ownership del Box per deallocare in sicurezza
                let boxed: Box<InnerVec<T>> = Box::from_raw(*self.ptr as *mut InnerVec<T>);

                let r = boxed.get_read();
                let w = boxed.get_write();
                let cap = boxed.get_cap();
                let buf_ptr = boxed.get_buf();

                let len = (w + cap - r) % cap;

                if std::mem::needs_drop::<T>() && len > 0 {
                    for i in 0..len {
                        let idx = (r + i) % cap;
                        ptr::drop_in_place(buf_ptr.add(idx).cast::<T>());
                    }
                }

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
