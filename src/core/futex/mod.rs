use std::sync::atomic::AtomicUsize;

pub type Futex = AtomicUsize;


#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "linux.rs"]
mod platform;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "watchos"))]
#[path = "macos.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "freebsd")]
#[path = "freebsd.rs"]
mod platform;

/// If atomic and value matches, wait until woken up.
/// This function might also return spuriously. Handle that case.
#[inline]
pub fn futex_wait(atomic: &AtomicUsize, value: usize) {
    platform::wait(atomic, value)
}

/// Wake one thread that is waiting on this atomic.
/// It's okay if the pointer dangles or is null.
#[inline]
pub fn futex_wake(atomic: *const AtomicUsize) {
    platform::wake_one(atomic);
}

/// Wake all threads that are waiting on this atomic.
/// It's okay if the pointer dangles or is null.
#[inline]
pub fn futex_wake_all(atomic: *const AtomicUsize) {
    platform::wake_all(atomic);
}


#[cfg(test)]
mod tests_futex {
    use crate::core::futex::{futex_wait, futex_wake, futex_wake_all};
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    #[test]
    fn wake_null() {
        futex_wake(std::ptr::null::<AtomicUsize>());
        futex_wake_all(std::ptr::null::<AtomicUsize>());
    }

    #[test]
    fn wake_nothing() {
        let a = AtomicUsize::new(0);
        futex_wake(&a);
        futex_wake_all(&a);
    }

    #[test]
    fn wait_unexpected() {
        let t = Instant::now();
        let a = AtomicUsize::new(0);
        futex_wait(&a, 1);
        assert!(t.elapsed().as_millis() < 100);
    }

    #[test]
    fn wait_wake() {
        let t = Instant::now();
        let a = AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                sleep(Duration::from_millis(100));
                a.store(1, Relaxed);
                futex_wake(&a);
            });
            while a.load(Relaxed) == 0 {
                futex_wait(&a, 0);
            }
            assert_eq!(a.load(Relaxed), 1);
            assert!((90..400).contains(&t.elapsed().as_millis()));
        });
    }
}
