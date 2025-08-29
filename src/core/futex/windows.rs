use std::sync::atomic::AtomicUsize;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::{WaitOnAddress, WakeByAddressAll, WakeByAddressSingle};

#[inline]
pub(crate) fn wait(a: &AtomicUsize, expected: usize) {
    let ptr: *const AtomicUsize = a;
    let expected_ptr: *const usize = &expected;
    unsafe { WaitOnAddress(ptr.cast(), expected_ptr.cast(), size_of::<usize>(), INFINITE) };
}

#[inline]
pub(crate) fn wake_one(ptr: *const AtomicUsize) {
    unsafe { WakeByAddressSingle(ptr.cast()) };
}

#[inline]
pub(crate) fn wake_all(ptr: *const AtomicUsize) {
    unsafe { WakeByAddressAll(ptr.cast()) };
}
