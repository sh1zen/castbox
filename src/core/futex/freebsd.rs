use core::sync::atomic::AtomicUsize;
use std::usize;

#[inline]
pub fn wait(a: &AtomicUsize, expected: usize) {
    let ptr: *const AtomicUsize = a;
    unsafe {
        libc::_umtx_op(
            ptr as *mut libc::c_void,
            libc::UMTX_OP_WAIT_UINT_PRIVATE,
            expected as libc::c_ulong,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    };
}

#[inline]
pub fn wake_one(ptr: *const AtomicUsize) {
    unsafe {
        libc::_umtx_op(
            ptr as *mut libc::c_void,
            libc::UMTX_OP_WAKE_PRIVATE,
            1 as libc::c_ulong,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    };
}

#[inline]
pub fn wake_all(ptr: *const AtomicUsize) {
    unsafe {
        libc::_umtx_op(
            ptr as *mut libc::c_void,
            libc::UMTX_OP_WAKE_PRIVATE,
            usize::MAX as libc::c_ulong,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    };
}
