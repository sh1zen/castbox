use crate::mutex::{Backoff, Mutex, WatchGuardRef};
use crossbeam_utils::CachePadded;
use std::alloc::{alloc_zeroed, handle_alloc_error, Layout};
use std::cell::UnsafeCell;
use std::fmt;
use std::iter::FromIterator;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Default initial capacity
const BLOCK_CAP: usize = 32;

/// Thread-safe Vec, clonable
#[repr(transparent)]
pub struct AtomicVec<T> {
    ptr: CachePadded<*const InnerVec<T>>,
}

struct Slot<T> {
    /// The value.
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    fn write(&self, value: T) {
        unsafe {
            self.value.get().write(MaybeUninit::new(value));
        }
    }

    fn read(&self) -> T {
        unsafe { self.value.get().read().assume_init() }
    }

    fn get_ref(&self) -> &T {
        unsafe { (&*self.value.get()).assume_init_ref() }
    }

    fn get_mut(&self) -> &T {
        unsafe { (&mut *self.value.get()).assume_init_mut() }
    }
}

struct Block<T> {
    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,
    /// Slots for values.
    slots: [Slot<T>; BLOCK_CAP],
}

#[inline(always)]
fn block_index(pos: usize) -> usize {
    pos >> 5
}

#[inline(always)]
fn index_in_block(pos: usize) -> usize {
    pos & (BLOCK_CAP - 1)
}

impl<T> Block<T> {
    const LAYOUT: Layout = {
        let layout = Layout::new::<Self>();
        assert!(
            layout.size() != 0,
            "Block should never be zero-sized, as it has an AtomicPtr field"
        );
        layout
    };

    /// Creates an empty block.
    fn new() -> Box<Self> {
        // SAFETY: layout is not zero-sized

        let ptr = unsafe { alloc_zeroed(Self::LAYOUT) };
        // Handle allocation failure

        if ptr.is_null() {
            handle_alloc_error(Self::LAYOUT)
        }
        // SAFETY: This is safe because:
        //  [1] `Block::next` (AtomicPtr) may be safely zero initialized.
        //  [2] `Block::slots` (Array) may be safely zero initialized because of [3, 4].
        //  [3] `Slot::value` (UnsafeCell) may be safely zero initialized because it
        //       holds a MaybeUninit.
        //  [4] `Slot::state` (AtomicUsize) may be safely zero initialized.
        // TODO: unsafe { Box::new_zeroed().assume_init() }

        unsafe { Box::from_raw(ptr.cast()) }
    }
}

struct InnerVec<T> {
    // padded fields for better cache isolation
    buf: Box<Block<T>>,
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
        let ptr = Box::into_raw(Box::new(InnerVec::new()));
        Self {
            ptr: CachePadded::new(ptr),
        }
    }

    /// init_with: costruisce e popola il buffer — operazione esclusiva
    pub fn init_with<F: FnMut() -> T>(cap: usize, mut initializer: F) -> Self {
        let vec = Self::new();
        let inner = vec.inner();

        for _ in 0..cap {
            vec.push(initializer());
        }

        inner.set_head(0);
        inner.set_len(cap);

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

        let cap = inner.get_cap();
        let len = inner.get_len();
        let head = inner.get_head();

        let write_pos = if len == 0 { 0 } else { (head + len) % cap };

        inner.get_slot(write_pos).write(value);

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

        let value = inner.get_slot(head).read();

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

        let value = inner.get_slot(pos).get_ref();

        Some(WatchGuardRef::new(value, inner.mutex.clone()))
    }

    pub fn reset_with(&self, new_cap: usize, mut initializer: impl FnMut() -> T) -> usize {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        // allocate and initialize new buffer. NOTE: if initializer panics here,
        // the new buffer may leak — this function is intentionally simplified.
        for i in 0..new_cap {
            let slot = inner.get_slot(i);
            slot.write(initializer());
        }

        // commit new buffer: it is fully initialized and contains `new_cap` elements
        inner.set_head(0);
        inner.set_len(new_cap);

        inner.mutex.unlock_exclusive();
        new_cap
    }
}

impl<T> AtomicVec<T> {
    /// Consuma i dati correnti e li restituisce come `Vec<T>`.
    /// Dopo la chiamata, l'AtomicVec risulta vuoto.
    pub fn as_vec(&self) -> Vec<T> {
        let inner = self.inner();
        inner.mutex.lock_exclusive();

        let head = inner.get_head();
        let len = inner.get_len();
        let cap = inner.get_cap();

        let mut out = Vec::with_capacity(len);

        for i in 0..len {
            let pos = (head + i) % cap;
            out.push(inner.get_slot(pos).read());
        }

        // reset indices: buffer ora considerato vuoto
        inner.set_head(0);
        inner.set_len(0);

        inner.mutex.unlock_exclusive();
        out
    }
}

impl<T> InnerVec<T> {
    fn new() -> Self {
        Self {
            buf: Block::<T>::new(),
            head: CachePadded::new(UnsafeCell::new(0)),
            len: CachePadded::new(UnsafeCell::new(0)),
            cap: CachePadded::new(UnsafeCell::new(BLOCK_CAP)),
            mutex: Mutex::new(),
            ref_count: CachePadded::new(AtomicUsize::new(1)),
        }
    }

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
    fn get_block(&self, n: usize) -> &Block<T> {
        let mut block = &*self.buf;
        for _ in 0..n {
            // carichiamo l'attuale next
            let mut next = block.next.load(Ordering::Acquire);
            if next.is_null() {
                // proviamo ad allocare un nuovo block e a installarlo atomically
                let new = Box::into_raw(Block::new());

                match block.next.compare_exchange(
                    null_mut(),
                    new,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // abbiamo vinto la gara: `new` è ora installato come next
                        // aggiorniamo la capacità (nota: questo muta l'UnsafeCell)
                        unsafe { *self.cap.get() += BLOCK_CAP };
                        next = new;
                    }
                    Err(actual) => {
                        // qualcun altro ha installato `actual` prima di noi:
                        // liberiamo la nostra allocazione per evitare leak
                        unsafe { drop(Box::from_raw(new)); }
                        next = actual;
                    }
                }
            }
            // a questo punto `next` non è null
            block = unsafe { &*next };
        }

        block
    }

    #[inline(always)]
    fn get_slot(&self, pos: usize) -> &Slot<T> {
        let block = self.get_block(block_index(pos));

        if let Some(slot) = block.slots.get(index_in_block(pos)) {
            slot
        } else {
            panic!("trying to get slot out of bounds");
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

                // Drop elements
                if std::mem::needs_drop::<T>() && len > 0 {
                    for i in 0..len {
                        let idx = (head + i) % cap;
                        let slot = boxed.get_slot(idx);
                        slot.value.get().drop_in_place();
                    }
                }

                // Deallocate blocks
                let mut block_ptr: *mut Block<T> = Box::into_raw(boxed.buf);

                while !block_ptr.is_null() {
                    let next = (*block_ptr).next.load(Ordering::Acquire);
                    // Deallocate current block
                    drop(Box::from_raw(block_ptr));
                    block_ptr = next;
                }
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
