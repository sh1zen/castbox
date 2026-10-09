//! Raw allocation helpers for explicit ownership transfer.
//!
//! These functions use `Box`-compatible allocations. Prefer ordinary boxes and
//! smart pointer constructors unless an external interface needs a raw pointer.
//! A raw pointer created here must be consumed exactly once: reconstruct a box,
//! transfer it with [`crate::AnyRef::from_owned_raw`] or
//! [`crate::Arw::from_owned_raw`], or release it with the appropriate helper.
//!
//! These payload pointers are distinct from opaque ownership pointers returned
//! by a smart pointer's `into_raw` method.

use std::alloc::{Layout, dealloc};

/// Calculate layout for `T` using the inner value's layout
pub(crate) fn memory_layout_for_t<T>(layout: Layout) -> Layout {
    // Calculate layout using the given value layout.
    Layout::new::<T>().extend(layout).unwrap().0.pad_to_align()
}

pub(crate) fn is_dangling<T: ?Sized>(ptr: *const T) -> bool {
    ptr.cast::<()>().addr() == usize::MAX
}

/// Allocates a value and transfers ownership to a raw pointer.
///
/// Equivalent to `Box::into_raw(Box::new(value))`. The returned pointer is valid
/// and aligned even for a zero-sized type. The caller is responsible for
/// transferring or releasing ownership exactly once; discarding the pointer
/// leaks the value and any allocation.
///
/// # Examples
/// ```
/// use castbox::utils::{create_raw_pointer, dealloc_raw_pointer};
/// let raw = create_raw_pointer(String::from("owned"));
/// // SAFETY: The pointer is valid, initialized, and still exclusively owned here.
/// unsafe { dealloc_raw_pointer(raw) };
/// ```
pub fn create_raw_pointer<T>(s: T) -> *mut T {
    Box::into_raw(Box::new(s))
}

/// Frees an allocation without dropping its value.
/// For a zero-sized type, there is no allocation to free.
///
/// # Safety
/// `raw` must come from `create_raw_pointer` or `Box::into_raw` for the same `T`,
/// and must not have been freed. The caller must have moved or dropped the value
/// already and must not access the allocation after this call.
///
/// # Examples
/// ```
/// use castbox::utils::{create_raw_pointer, dealloc_layout};
/// let raw = create_raw_pointer(String::from("moved"));
/// // SAFETY: We own the initialized value and move it out exactly once.
/// let value = unsafe { raw.read() };
/// // SAFETY: The value has been moved out and this allocation has not been freed.
/// unsafe { dealloc_layout(raw) };
/// assert_eq!(value, "moved");
/// ```
///
/// ```compile_fail
/// let raw = castbox::utils::create_raw_pointer(42);
/// castbox::utils::dealloc_layout(raw); // requires an unsafe block
/// ```
#[inline]
pub unsafe fn dealloc_layout<T>(raw: *mut T) {
    let layout = Layout::new::<T>();
    if layout.size() != 0 {
        unsafe { dealloc(raw.cast(), layout) };
    }
}

/// Drops a value and frees its allocation, including zero-sized values.
///
/// # Safety
/// `raw` must be a valid, unconsumed pointer from `create_raw_pointer` or
/// `Box::into_raw` for the same `T`, with exclusive ownership transferred here.
/// After this call, do not access the pointer or release its ownership again.
#[inline]
pub unsafe fn dealloc_raw_pointer<T>(raw: *mut T) {
    unsafe { drop(Box::from_raw(raw)) };
}
