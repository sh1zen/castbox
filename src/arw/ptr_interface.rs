use crate::arw::inner::ArwInner;

pub(crate) trait PtrInterface<T>: Sized {
    fn get_mut_inner_ptr(&self) -> *mut ArwInner<T>;

    unsafe fn from_inner_in(ptr: *mut ArwInner<T>) -> Self;

    unsafe fn from_inner(ptr: *mut ArwInner<T>) -> Self {
        unsafe { Self::from_inner_in(ptr) }
    }
}
