mod tests_any_ref {
    use crate::anyref::{AnyRef, WeakAnyRef};
    use crossync::sync::WatchGuardRef;
    use std::any::TypeId;
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    // ==================== Basic Construction Tests ====================

    #[test]
    fn new_with_primitive_types() {
        let i32_ref = AnyRef::new(42i32);
        let f64_ref = AnyRef::new(3.14f64);
        let bool_ref = AnyRef::new(true);
        let char_ref = AnyRef::new('x');

        assert_eq!(*i32_ref.as_ref::<i32>(), 42i32);
        assert!((f64_ref.as_ref::<f64>().clone() - 3.14f64).abs() < f64::EPSILON);
        assert!(*bool_ref.as_ref::<bool>());
        assert_eq!(*char_ref.as_ref::<char>(), 'x');
    }

    #[test]
    fn new_with_complex_types() {
        let vec_ref = AnyRef::new(vec![1, 2, 3, 4, 5]);
        let string_ref = AnyRef::new(String::from("hello world"));
        let option_ref = AnyRef::new(Some(42));

        assert_eq!(*vec_ref.as_ref::<Vec<i32>>(), vec![1, 2, 3, 4, 5]);
        assert_eq!(*string_ref.as_ref::<String>(), "hello world");
        assert_eq!(*option_ref.as_ref::<Option<i32>>(), Some(42));
    }

    #[test]
    fn new_with_zero_sized_type() {
        #[derive(Debug, PartialEq)]
        struct ZST;
        let zst_ref = AnyRef::new(ZST);
        assert_eq!(*zst_ref.as_ref::<ZST>(), ZST);
    }

    // ==================== Type Checking Tests ====================

    #[test]
    fn type_id_is_correct() {
        let x = AnyRef::new(42u32);
        assert_eq!(x.inner().type_id, TypeId::of::<u32>());
        assert_ne!(x.inner().type_id, TypeId::of::<i32>());
    }

    #[test]
    fn type_name_is_correct() {
        let x = AnyRef::new(42i32);
        assert_eq!(x.type_name(), "i32");
    }

    #[test]
    fn try_downcast_ref_wrong_type_returns_none() {
        let x = AnyRef::new(42i32);
        assert!(x.try_downcast_ref::<u32>().is_none());
        assert!(x.try_downcast_ref::<String>().is_none());
    }

    #[test]
    #[should_panic(expected = "Downcast failed")]
    fn as_ref_wrong_type_panics() {
        let x = AnyRef::new(42i32);
        let _ = x.as_ref::<String>();
    }

    #[test]
    #[should_panic(expected = "Downcast mut failed")]
    fn as_mut_wrong_type_panics() {
        let x = AnyRef::new(42i32);
        let _ = x.as_mut::<String>();
    }

    // ==================== Reference Counting Tests ====================

    #[test]
    fn strong_count_increments_on_clone() {
        let x = AnyRef::new(42);
        assert_eq!(AnyRef::strong_count(&x), 1);
        let y = x.clone();
        assert_eq!(AnyRef::strong_count(&x), 2);
        assert_eq!(AnyRef::strong_count(&y), 2);
        drop(y);
        assert_eq!(AnyRef::strong_count(&x), 1);
    }

    #[test]
    fn weak_count_increments_on_downgrade() {
        let x = AnyRef::new(42);
        assert_eq!(AnyRef::weak_count(&x), 0);
        let w1 = x.downgrade();
        assert_eq!(AnyRef::weak_count(&x), 1);
        let w2 = w1.clone();
        assert_eq!(AnyRef::weak_count(&x), 2);
        drop(w1);
        drop(w2);
        assert_eq!(AnyRef::weak_count(&x), 0);
    }

    #[test]
    fn weak_strong_count_methods() {
        let x = AnyRef::new(42);
        let w = x.downgrade();
        assert_eq!(w.strong_count(), 1);
        assert_eq!(w.weak_count(), 1);
        drop(x);
        assert_eq!(w.strong_count(), 0);
    }

    // ==================== is_unique Tests ====================

    #[test]
    fn is_unique_with_single_strong_ref() {
        let x = AnyRef::new(42);
        assert!(AnyRef::is_unique(&x));
    }

    #[test]
    fn is_unique_with_multiple_strong_refs() {
        let x = AnyRef::new(42);
        let _y = x.clone();
        assert!(!AnyRef::is_unique(&x));
    }

    // ==================== ptr_eq Tests ====================

    #[test]
    fn ptr_eq_same_allocation() {
        let x = AnyRef::new(42);
        let y = x.clone();
        assert!(AnyRef::ptr_eq(&x, &y));
    }

    #[test]
    fn ptr_eq_different_allocations() {
        let x = AnyRef::new(42);
        let y = AnyRef::new(42);
        assert!(!AnyRef::ptr_eq(&x, &y));
    }

    // ==================== try_unwrap Tests ====================

    #[test]
    fn try_unwrap_success_with_single_ref() {
        let x = AnyRef::new(String::from("hello"));
        let result = AnyRef::try_unwrap::<String>(x);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn try_unwrap_fails_with_multiple_refs() {
        let x = AnyRef::new(42i32);
        let y = x.clone();
        let result = AnyRef::try_unwrap::<i32>(x);
        assert!(result.is_err());
        let returned = result.unwrap_err();
        assert_eq!(*returned.as_ref::<i32>(), 42);
        drop(y);
    }

    #[test]
    fn try_unwrap_fails_with_weak_refs() {
        let x = AnyRef::new(42i32);
        let w = x.downgrade();
        let result = AnyRef::try_unwrap::<i32>(x);
        assert!(result.is_err());
        drop(w);
    }

    #[test]
    fn try_unwrap_wrong_type_returns_err() {
        let x = AnyRef::new(42i32);
        let result = AnyRef::try_unwrap::<String>(x);
        assert!(result.is_err());
        let returned = result.unwrap_err();
        assert_eq!(*returned.as_ref::<i32>(), 42);
    }

    #[test]
    fn try_unwrap_drops_correctly() {
        static DROP_COUNT: AtomicU8 = AtomicU8::new(0);

        struct DropTracker(i32);
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);
        let x = AnyRef::new(DropTracker(42));
        let val = AnyRef::try_unwrap::<DropTracker>(x).unwrap();
        assert_eq!(val.0, 42);
        assert_eq!(DROP_COUNT.load(SeqCst), 0);
        drop(val);
        assert_eq!(DROP_COUNT.load(SeqCst), 1);
    }

    // ==================== map Tests ====================

    #[test]
    fn map_transforms_value() {
        let x = AnyRef::new(5i32);
        let y = x.map(|v: WatchGuardRef<'_, i32>| (*v * 2) as u64);
        assert_eq!(*y.as_ref::<u64>(), 10u64);
    }

    // ==================== fill Tests ====================

    #[test]
    fn fill_replaces_value() {
        let x = AnyRef::new(42i32);
        let x = AnyRef::fill(x, 100i32);
        assert_eq!(*x.as_ref::<i32>(), 100);
    }

    #[test]
    fn fill_changes_type() {
        let x = AnyRef::new(42i32);
        let x = AnyRef::fill(x, String::from("hello"));
        assert_eq!(*x.as_ref::<String>(), "hello");
    }

    // ==================== default Tests ====================

    #[test]
    fn default_with_creates_default_value() {
        let x = AnyRef::default_with::<i32>();
        assert_eq!(*x.as_ref::<i32>(), 0);
        let y = AnyRef::default_with::<String>();
        assert_eq!(*y.as_ref::<String>(), "");
    }

    // ==================== From Implementations Tests ====================

    #[test]
    fn from_str_creates_owned_string() {
        let x = AnyRef::from("hello");
        assert_eq!(*x.as_ref::<String>(), "hello");
    }

    #[test]
    fn from_box_takes_ownership() {
        let boxed = Box::new(42i32);
        let x = AnyRef::from(boxed);
        assert_eq!(*x.as_ref::<i32>(), 42);
    }

    // ==================== into_raw / from_raw Tests ====================

    #[test]
    fn into_raw_and_from_raw_roundtrip() {
        let x = AnyRef::new(String::from("test"));
        let raw = AnyRef::into_raw(x);
        let y = unsafe { AnyRef::from_raw(raw) };
        assert_eq!(*y.as_ref::<String>(), "test");
    }

    #[test]
    fn into_raw_prevents_drop_until_reconstructed() {
        static DROP_COUNT: AtomicU8 = AtomicU8::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);
        let x = AnyRef::new(DropTracker);
        let raw = AnyRef::into_raw(x);
        assert_eq!(DROP_COUNT.load(SeqCst), 0);
        let y = unsafe { AnyRef::from_raw(raw) };
        assert_eq!(DROP_COUNT.load(SeqCst), 0);
        drop(y);
        assert_eq!(DROP_COUNT.load(SeqCst), 1);
    }

    // ==================== WeakAnyRef Tests ====================

    #[test]
    fn weak_new_creates_dangling() {
        let w = WeakAnyRef::new();
        assert!(w.upgrade().is_none());
        assert_eq!(w.strong_count(), 0);
    }

    #[test]
    fn weak_default_creates_dangling() {
        let w: WeakAnyRef = Default::default();
        assert!(w.upgrade().is_none());
    }

    #[test]
    fn weak_upgrade_after_strong_drop_fails() {
        let weak: WeakAnyRef;
        {
            let x = AnyRef::new(42);
            weak = x.downgrade();
            assert!(weak.upgrade().is_some());
        }
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn weak_clone_of_dangling() {
        let w1 = WeakAnyRef::new();
        let w2 = w1.clone();
        assert!(w1.upgrade().is_none());
        assert!(w2.upgrade().is_none());
    }

    #[test]
    fn multiple_upgrades_work() {
        let x = AnyRef::new(42);
        let w = x.downgrade();
        let y1 = w.upgrade().unwrap();
        let y2 = w.upgrade().unwrap();
        assert_eq!(AnyRef::strong_count(&x), 3);
        assert_eq!(*y1.as_ref::<i32>(), 42);
        assert_eq!(*y2.as_ref::<i32>(), 42);
    }

    // ==================== Drop Behavior Tests ====================

    #[test]
    fn drop_is_called_exactly_once() {
        static DROP_COUNT: AtomicU8 = AtomicU8::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);
        let x = AnyRef::new(DropTracker);
        let y = x.clone();
        let z = x.clone();
        let w = x.downgrade();

        drop(x);
        assert_eq!(DROP_COUNT.load(SeqCst), 0);
        drop(y);
        assert_eq!(DROP_COUNT.load(SeqCst), 0);
        drop(z);
        assert_eq!(DROP_COUNT.load(SeqCst), 1);
        assert!(w.upgrade().is_none());
    }

    // ==================== Concurrent Tests ====================

    #[test]
    fn concurrent_reads() {
        let x = AnyRef::new(vec![1, 2, 3, 4, 5]);
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = vec![];

        for _ in 0..10 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..100 {
                    let v = x_clone.as_ref::<Vec<i32>>();
                    assert_eq!(v.len(), 5);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn concurrent_writes() {
        let x = AnyRef::new(0i32);
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = vec![];

        for _ in 0..10 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..10 {
                    let mut v = x_clone.as_mut::<i32>();
                    *v += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*x.as_ref::<i32>(), 100);
    }

    #[test]
    fn concurrent_clone_and_drop() {
        let x = AnyRef::new(42);
        let barrier = Arc::new(Barrier::new(50));
        let mut handles = vec![];

        for _ in 0..50 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                let _y = x_clone.clone();
                thread::sleep(Duration::from_micros(10));
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(AnyRef::strong_count(&x), 1);
    }

    #[test]
    fn concurrent_upgrade_during_drop() {
        for _ in 0..20 {
            let x = AnyRef::new(42);
            let weak = x.downgrade();
            let weak_clone = weak.clone();

            let handle = thread::spawn(move || {
                for _ in 0..50 {
                    if let Some(strong) = weak_clone.upgrade() {
                        assert_eq!(*strong.as_ref::<i32>(), 42);
                    }
                }
            });

            drop(x);
            handle.join().unwrap();
        }
    }

    // ==================== Mutation Tests ====================

    #[test]
    fn mutation_visible_through_clones() {
        let x = AnyRef::new(vec![1, 2, 3]);
        let y = x.clone();
        x.as_mut::<Vec<i32>>().push(4);
        assert_eq!(*y.as_ref::<Vec<i32>>(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn mutation_visible_through_weak() {
        let x = AnyRef::new(String::from("hello"));
        let w = x.downgrade();
        x.as_mut::<String>().push_str(" world");
        let upgraded = w.upgrade().unwrap();
        assert_eq!(*upgraded.as_ref::<String>(), "hello world");
    }

    // ==================== Edge Cases ====================

    #[test]
    fn many_clones_and_drops() {
        let x = AnyRef::new(String::from("test"));
        let mut refs: Vec<AnyRef> = vec![];
        for _ in 0..100 {
            refs.push(x.clone());
        }
        assert_eq!(AnyRef::strong_count(&x), 101);
        refs.clear();
        assert_eq!(AnyRef::strong_count(&x), 1);
    }

    #[test]
    fn many_weak_refs() {
        let x = AnyRef::new(42);
        let mut weaks: Vec<WeakAnyRef> = vec![];
        for _ in 0..100 {
            weaks.push(x.downgrade());
        }
        assert_eq!(AnyRef::weak_count(&x), 100);
        for w in &weaks {
            assert!(w.upgrade().is_some());
        }
        weaks.clear();
        assert_eq!(AnyRef::weak_count(&x), 0);
    }

    #[test]
    fn fill_after_try_unwrap_failure() {
        let x = AnyRef::new(42i32);
        let y = x.clone();
        let x = AnyRef::try_unwrap::<i32>(x).unwrap_err();
        let x = AnyRef::fill(x, 100i32);
        assert_eq!(*x.as_ref::<i32>(), 100);
        assert_eq!(*y.as_ref::<i32>(), 100);
    }

    // ==================== as_cast_ptr Tests ====================

    #[test]
    fn as_cast_ptr_returns_valid_pointer() {
        let x = AnyRef::new(42i32);
        let ptr = unsafe { x.as_cast_ptr::<i32>() };
        unsafe {
            assert_eq!(*ptr, 42);
        }
    }

    // ==================== Debug/Display Tests ====================

    #[test]
    fn debug_format_contains_type_info() {
        let x = AnyRef::new(42i32);
        let debug_str = format!("{:?}", x);
        assert!(debug_str.contains("AnyRef"));
        assert!(debug_str.contains("i32"));
    }
}
