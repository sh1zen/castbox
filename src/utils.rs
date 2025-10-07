use std::alloc::{Layout, alloc, dealloc};
use std::{ptr, thread};
use std::time::Instant;

/// Calculate layout for `T` using the inner value's layout
pub(crate) fn memory_layout_for_t<T>(layout: Layout) -> Layout {
    // Calculate layout using the given value layout.
    Layout::new::<T>().extend(layout).unwrap().0.pad_to_align()
}

pub(crate) fn is_dangling<T: ?Sized>(ptr: *const T) -> bool {
    ptr.cast::<()>().addr() == usize::MAX
}

pub fn create_raw_pointer<T>(s: T) -> *mut T {
    // Layout of T
    let layout = Layout::new::<T>();

    // Alloc memory space
    let raw: *mut T = unsafe {
        let mem_ptr = alloc(layout) as *mut T;
        if mem_ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr::write(mem_ptr, s);
        mem_ptr
    };
    raw
}

#[inline]
pub fn dealloc_layout<T>(raw: *mut T) {
    unsafe {
        dealloc(raw as *mut u8, Layout::new::<T>());
    }
}

#[inline]
pub unsafe fn dealloc_raw_pointer<T>(raw: *mut T) {
    unsafe {
        ptr::drop_in_place(raw);
        dealloc_layout::<T>(raw);
    }
}

pub fn wait_until(deadline: Instant, pred: impl Fn() -> bool) -> bool {
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::yield_now();
        std::hint::spin_loop();
    }
    false
}
