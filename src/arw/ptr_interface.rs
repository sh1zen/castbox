use crate::arw::inner::ArwInner;
use crate::utils::is_dangling;
use std::mem::offset_of;

pub(crate) trait PtrInterface<T>
where
    Self: Sized,
{
    fn get_mut_inner_ptr(&self) -> *mut ArwInner<T>;

    unsafe fn from_inner_in(ptr: *mut ArwInner<T>) -> Self;

    #[inline]
    unsafe fn from_ptr_in(ptr: *mut ArwInner<T>) -> Self {
        unsafe { Self::from_inner_in(ptr) }
    }

    unsafe fn from_ptr(ptr: *mut ArwInner<T>) -> Self {
        unsafe { Self::from_ptr_in(ptr) }
    }

    unsafe fn from_inner(ptr: *mut ArwInner<T>) -> Self {
        unsafe { Self::from_inner_in(ptr) }
    }

    #[inline]
    unsafe fn from_raw_in(ptr: *const T) -> Self {
        let inner_ptr = if is_dangling(ptr) {
            // This is a dangling Weak.
            ptr as *mut ArwInner<T>
        } else {
            let obj = ptr as *const u8;
            let data_offset = offset_of!(ArwInner<T>, val);

            // SAFETY: we assume the dyn Any points to ARWInner.data
            unsafe { obj.offset(-(data_offset as isize)) as *mut ArwInner<T> }
        };

        unsafe { Self::from_ptr_in(inner_ptr) }
    }
}
