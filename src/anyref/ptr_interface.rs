use crate::anyref::inner::AnyRefInner;

pub(crate) trait PtrInterface: Sized {
    fn get_mut_inner_ptr(&self) -> *mut AnyRefInner;

    unsafe fn from_inner_in(ptr: *mut AnyRefInner) -> Self;

    unsafe fn from_inner(ptr: *mut AnyRefInner) -> Self {
        unsafe { Self::from_inner_in(ptr) }
    }
}
