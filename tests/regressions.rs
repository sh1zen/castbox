use castbox::utils::{create_raw_pointer, dealloc_layout, dealloc_raw_pointer};
use castbox::{AnyRef, Arw, WeakAnyRef, WeakArw};
use std::any::type_name;
use std::panic::catch_unwind;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn failed_arw_unwrap_never_interrupts_weak_upgrade() {
    let a = Arw::new(42usize);
    let weak = a.downgrade();
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let worker_stop = stop.clone();
    let worker_barrier = barrier.clone();
    let worker = thread::spawn(move || {
        let mut a = a;
        worker_barrier.wait();
        while !worker_stop.load(Ordering::Relaxed) {
            a = Arw::try_unwrap(a).expect_err("a weak reference still exists");
            thread::yield_now();
        }
        a
    });
    barrier.wait();
    let misses = (0..1000).filter(|_| weak.upgrade().is_none()).count();
    stop.store(true, Ordering::Relaxed);
    let a = worker.join().unwrap();
    assert_eq!(misses, 0);
    assert_eq!(*a.as_ref(), 42);
}

#[test]
fn failed_anyref_unwrap_never_interrupts_weak_upgrade() {
    let a = AnyRef::new(42usize);
    let weak = a.downgrade();
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let worker_stop = stop.clone();
    let worker_barrier = barrier.clone();
    let worker = thread::spawn(move || {
        let mut a = a;
        worker_barrier.wait();
        while !worker_stop.load(Ordering::Relaxed) {
            a = AnyRef::try_unwrap::<usize>(a).expect_err("a weak reference still exists");
            thread::yield_now();
        }
        a
    });
    barrier.wait();
    let misses = (0..1000).filter(|_| weak.upgrade().is_none()).count();
    stop.store(true, Ordering::Relaxed);
    let a = worker.join().unwrap();
    assert_eq!(misses, 0);
    assert_eq!(*a.as_ref::<usize>(), 42);
}

#[test]
fn arw_pointer_formatting_works_while_mutably_borrowed() {
    let a = Arw::new(42usize);
    let _guard = a.as_mut();
    let raw = a.clone().into_raw();
    assert_eq!(format!("{a:p}"), format!("{raw:p}"));
    drop(unsafe { Arw::<usize>::from_raw(raw) });
}

#[test]
fn anyref_pointer_formatting_works_while_mutably_borrowed() {
    let a = AnyRef::new(42usize);
    let _guard = a.as_mut::<usize>();
    let raw = a.clone().into_raw();
    assert_eq!(format!("{a:p}"), format!("{raw:p}"));
    drop(unsafe { AnyRef::from_raw(raw) });
}

#[test]
fn replacement_updates_type_name_and_debug_for_all_clones() {
    let a = AnyRef::new(42i32);
    let b = a.clone();
    let a = AnyRef::fill(a, String::from("new value"));
    assert_eq!(b.type_name(), type_name::<String>());
    assert!(format!("{b:?}").contains(type_name::<String>()));
    assert_eq!(*a.as_ref::<String>(), "new value");
    assert!(b.try_downcast_ref::<i32>().is_none());
    assert!(b.try_downcast_mut::<i32>().is_none());
}

#[test]
fn metadata_is_accessible_while_a_value_guard_is_held() {
    let a = AnyRef::new(42i32);
    let _guard = a.as_mut::<i32>();
    assert_eq!(a.type_name(), type_name::<i32>());
    assert!(format!("{a:?}").contains("i32"));
}

#[test]
fn concurrent_replacement_and_downcast() {
    let a = AnyRef::new(0i32);
    let b = a.clone();
    let barrier = Arc::new(Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = thread::spawn(move || {
        let mut b = b;
        writer_barrier.wait();
        for i in 0..500 {
            b = AnyRef::fill(b, i);
            b = AnyRef::fill(b, String::from("value"));
        }
    });
    barrier.wait();
    for _ in 0..1000 {
        if let Some(v) = a.try_downcast_ref::<i32>() {
            assert!((0..500).contains(&*v));
        }
        if let Some(v) = a.try_downcast_mut::<String>() {
            assert_eq!(*v, "value");
        }
        assert!([type_name::<i32>(), type_name::<String>()].contains(&a.type_name()));
    }
    writer.join().unwrap();
    assert_eq!(*a.as_ref::<String>(), "value");
}

#[test]
fn arw_fill_shares_updates_with_concurrent_readers() {
    let a = Arw::new(0usize);
    let b = a.clone();
    let barrier = Arc::new(Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = thread::spawn(move || {
        let mut b = b;
        writer_barrier.wait();
        for i in 0..500 {
            b = Arw::fill(b, i);
        }
    });
    barrier.wait();
    for _ in 0..1000 {
        assert!(*a.as_ref() < 500);
        let clone = a.clone();
        assert!(*clone.as_ref() < 500);
    }
    writer.join().unwrap();
    assert_eq!(*a.as_ref(), 499);
}

struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("old payload destructor");
    }
}

#[test]
fn replacement_remains_consistent_when_old_destructor_panics() {
    let a = AnyRef::new(PanicOnDrop);
    let b = a.clone();
    assert!(catch_unwind(move || AnyRef::fill(a, String::from("replacement"))).is_err());
    assert_eq!(b.type_name(), type_name::<String>());
    assert!(b.try_downcast_ref::<PanicOnDrop>().is_none());
    assert_eq!(*b.as_ref::<String>(), "replacement");
    assert_eq!(AnyRef::try_unwrap::<String>(b).unwrap(), "replacement");
}

#[test]
fn old_destructor_can_access_the_replacement_through_a_clone() {
    struct Observer(AnyRef, Arc<AtomicUsize>);
    impl Drop for Observer {
        fn drop(&mut self) {
            self.1.store(*self.0.as_ref::<usize>(), Ordering::SeqCst);
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let a = AnyRef::new(0usize);
    let observer = Observer(a.clone(), seen.clone());
    let a = AnyRef::fill(a, observer);
    let a = AnyRef::fill(a, 42usize);
    assert_eq!(seen.load(Ordering::SeqCst), 42);
    assert_eq!(AnyRef::strong_count(&a), 1);
}

#[test]
fn weak_counts_exclude_implicit_reference_before_and_after_drop() {
    let a = AnyRef::new(42);
    let w = a.downgrade();
    let w2 = w.clone();
    assert_eq!(w.weak_count(), 2);
    assert_eq!(w.weak_count(), AnyRef::weak_count(&a));
    drop(a);
    assert_eq!(w.weak_count(), 2);
    drop(w2);
    assert_eq!(w.weak_count(), 1);
    assert!(w.upgrade().is_none());

    let a = Arw::new(42);
    let w = a.downgrade();
    let w2 = w.clone();
    assert_eq!(w.weak_count(), 2);
    assert_eq!(w.weak_count(), Arw::weak_count(&a));
    drop(a);
    assert_eq!(w.weak_count(), 2);
    drop(w2);
    assert_eq!(w.weak_count(), 1);
    assert!(w.upgrade().is_none());
}

struct DropTracker(Arc<AtomicUsize>);

#[test]
fn weak_counts_exclude_implicit_owner_during_payload_destruction() {
    struct AnyObserver(WeakAnyRef, Arc<AtomicUsize>);
    impl Drop for AnyObserver {
        fn drop(&mut self) {
            self.1.store(self.0.weak_count(), Ordering::SeqCst);
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let a = AnyRef::new(());
    let weak = a.downgrade();
    let a = AnyRef::fill(a, AnyObserver(weak, seen.clone()));
    drop(a);
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    struct ArwObserver(WeakArw<ArwObserver>, Arc<AtomicUsize>);
    impl Drop for ArwObserver {
        fn drop(&mut self) {
            self.1.store(self.0.weak_count(), Ordering::SeqCst);
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let a = Arw::new(ArwObserver(WeakArw::new(), seen.clone()));
    a.as_mut().0 = a.downgrade();
    drop(a);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

impl Drop for DropTracker {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn owned_raw_constructors_take_allocation_and_drop_value_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let raw = create_raw_pointer(DropTracker(drops.clone()));
    let a = unsafe { AnyRef::from_owned_raw(raw) };
    let clone = a.clone();
    drop(a);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(clone);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let raw = create_raw_pointer(DropTracker(drops.clone()));
    let a = unsafe { Arw::from_owned_raw(raw) };
    drop(a);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn opaque_raw_roundtrip_preserves_identity_counts_and_weak_references() {
    let drops = Arc::new(AtomicUsize::new(0));
    let a = AnyRef::new(DropTracker(drops.clone()));
    let clone = a.clone();
    let weak = a.downgrade();
    let raw: *const () = a.into_raw();
    assert_eq!(weak.strong_count(), 2);
    let a = unsafe { AnyRef::from_raw(raw) };
    assert!(AnyRef::ptr_eq(&a, &clone));
    drop(clone);
    drop(a);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_none());

    let a = Arw::new(DropTracker(drops.clone()));
    let clone = a.clone();
    let weak = a.downgrade();
    let raw: *const () = a.into_raw();
    assert_eq!(weak.strong_count(), 2);
    let a = unsafe { Arw::<DropTracker>::from_raw(raw) };
    assert!(Arw::ptr_eq(&a, &clone));
    drop(clone);
    drop(a);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(weak.upgrade().is_none());
}

#[test]
fn zero_sized_allocations_keep_alignment_and_drop_once() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[repr(align(4096))]
    struct AlignedZst;
    impl Drop for AlignedZst {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    assert_eq!(std::mem::size_of::<AlignedZst>(), 0);
    let raw = create_raw_pointer(AlignedZst);
    assert!(!raw.is_null());
    assert_eq!(raw.addr() % std::mem::align_of::<AlignedZst>(), 0);
    unsafe { dealloc_raw_pointer(raw) };
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);

    let raw = create_raw_pointer(AlignedZst);
    let value = unsafe { raw.read() };
    unsafe { dealloc_layout(raw) };
    drop(value);
    assert_eq!(DROPS.load(Ordering::SeqCst), 2);

    let raw = create_raw_pointer(AlignedZst);
    drop(unsafe { AnyRef::from_owned_raw(raw) });
    let raw = create_raw_pointer(AlignedZst);
    drop(unsafe { Arw::from_owned_raw(raw) });
    assert_eq!(DROPS.load(Ordering::SeqCst), 4);
}
