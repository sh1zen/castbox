mod tests_arw {
    use crate::arw::{Arw, WeakArw};
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    // ==================== Basic Construction ====================

    #[test]
    fn new_with_primitive_types() {
        let i32_ref = Arw::new(42i32);
        let f64_ref = Arw::new(3.14f64);
        let bool_ref = Arw::new(true);

        assert_eq!(*i32_ref.as_ref(), 42i32);
        assert!((*f64_ref.as_ref() - 3.14f64).abs() < f64::EPSILON);
        assert!(*bool_ref.as_ref());
    }

    #[test]
    fn new_with_complex_types() {
        let vec_ref = Arw::new(vec![1, 2, 3, 4, 5]);
        let string_ref = Arw::new(String::from("hello world"));

        assert_eq!(*vec_ref.as_ref(), vec![1, 2, 3, 4, 5]);
        assert_eq!(*string_ref.as_ref(), "hello world");
    }

    #[test]
    fn new_with_zero_sized_type() {
        #[derive(Debug, PartialEq)]
        struct ZST;

        let zst_ref = Arw::new(ZST);
        assert_eq!(*zst_ref.as_ref(), ZST);
        assert_eq!(Arw::strong_count(&zst_ref), 1);
    }

    // ==================== Reference Counting ====================

    #[test]
    fn strong_count_clone_drop() {
        let x = Arw::new(42);
        assert_eq!(Arw::strong_count(&x), 1);

        let y = x.clone();
        assert_eq!(Arw::strong_count(&x), 2);

        drop(y);
        assert_eq!(Arw::strong_count(&x), 1);
    }

    #[test]
    fn weak_count_operations() {
        let x = Arw::new(42);
        assert_eq!(Arw::weak_count(&x), 0);

        let w1 = x.downgrade();
        assert_eq!(Arw::weak_count(&x), 1);

        let w2 = w1.clone();
        assert_eq!(Arw::weak_count(&x), 2);

        drop(w1);
        drop(w2);
        assert_eq!(Arw::weak_count(&x), 0);
    }

    // ==================== is_unique ====================

    #[test]
    fn is_unique_tests() {
        let x = Arw::new(42);
        assert!(Arw::is_unique(&x));

        let _y = x.clone();
        assert!(!Arw::is_unique(&x));
    }

    // ==================== ptr_eq ====================

    #[test]
    fn ptr_eq_tests() {
        let x = Arw::new(42);
        let y = x.clone();
        let z = Arw::new(42);

        assert!(Arw::ptr_eq(&x, &y));
        assert!(!Arw::ptr_eq(&x, &z));
    }

    // ==================== try_unwrap ====================

    #[test]
    fn try_unwrap_success() {
        let x = Arw::new(String::from("hello"));
        let result = Arw::try_unwrap(x);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn try_unwrap_fails_with_clones() {
        let x = Arw::new(42i32);
        let y = x.clone();

        let result = Arw::try_unwrap(x);
        assert!(result.is_err());
        drop(y);
    }

    #[test]
    fn try_unwrap_drops_correctly() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker(i32);
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);

        {
            let x = Arw::new(DropTracker(42));
            let val = Arw::try_unwrap(x).unwrap();
            // Value extracted, not yet dropped
            assert_eq!(
                DROP_COUNT.load(SeqCst),
                0,
                "Value dropped prematurely during try_unwrap"
            );
            assert_eq!(val.0, 42);
            // val goes out of scope here and should be dropped
        }

        assert_eq!(
            DROP_COUNT.load(SeqCst),
            1,
            "Value should be dropped exactly once"
        );
    }

    #[test]
    fn try_unwrap_no_double_drop() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
        const CANARY: u64 = 0xCAFEBABE;

        struct DoubleDropDetector {
            canary: u64,
            id: usize,
        }

        impl DoubleDropDetector {
            fn new(id: usize) -> Self {
                Self { canary: CANARY, id }
            }
        }

        impl Drop for DoubleDropDetector {
            fn drop(&mut self) {
                assert_eq!(
                    self.canary, CANARY,
                    "Double drop detected for id {}",
                    self.id
                );
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);

        let x = Arw::new(DoubleDropDetector::new(1));
        let val = Arw::try_unwrap(x).unwrap();
        assert_eq!(val.id, 1);
        drop(val);

        assert_eq!(DROP_COUNT.load(SeqCst), 1);
    }

    // ==================== map ====================

    #[test]
    fn map_transforms_value() {
        let x = Arw::new(5i32);
        let y = x.map(|v: &i32| (*v * 2) as u64);
        assert_eq!(*y.as_ref(), 10u64);
    }

    // ==================== fill ====================

    #[test]
    fn fill_replaces_value() {
        let x = Arw::new(42i32);
        let y = x.clone();
        let x = Arw::fill(x, 100i32);

        assert_eq!(*x.as_ref(), 100);
        assert_eq!(*y.as_ref(), 100);
    }

    // ==================== default ====================

    #[test]
    fn default_creates_default_value() {
        let x: Arw<i32> = Arw::default();
        assert_eq!(*x.as_ref(), 0);
    }

    // ==================== into_raw / from_raw ====================

    #[test]
    fn into_raw_from_raw_roundtrip() {
        let x = Arw::new(String::from("test"));
        let raw = Arw::into_raw(x);
        let y = unsafe { Arw::from_raw(raw) };
        assert_eq!(*y.as_ref(), "test");
    }

    // ==================== WeakArw ====================

    #[test]
    fn weak_new_is_dangling() {
        let w: WeakArw<i32> = WeakArw::new();
        assert!(w.upgrade().is_none());
        assert_eq!(w.strong_count(), 0);
    }

    #[test]
    fn weak_upgrade_after_drop_fails() {
        let weak: WeakArw<i32>;
        {
            let x = Arw::new(42);
            weak = x.downgrade();
            assert!(weak.upgrade().is_some());
        }
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn weak_survives_strong_drops() {
        let weak: WeakArw<String>;
        {
            let x = Arw::new(String::from("test"));
            weak = x.downgrade();
        }

        assert!(weak.upgrade().is_none());
        assert_eq!(weak.strong_count(), 0);
        assert_eq!(weak.weak_count(), 1);
    }

    // ==================== Drop Behavior ====================

    #[test]
    fn drop_called_once() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);

        let x = Arw::new(DropTracker);
        let y = x.clone();
        let _w = x.downgrade();

        drop(x);
        assert_eq!(DROP_COUNT.load(SeqCst), 0);

        drop(y);
        assert_eq!(DROP_COUNT.load(SeqCst), 1);
    }

    // ==================== Mutation ====================

    #[test]
    fn mutation_visible_through_clones() {
        let x = Arw::new(vec![1, 2, 3]);
        let y = x.clone();

        x.as_mut().push(4);
        assert_eq!(*y.as_ref(), vec![1, 2, 3, 4]);
    }

    // ==================== Concurrent Tests ====================

    #[test]
    fn concurrent_reads() {
        let x = Arw::new(vec![1, 2, 3, 4, 5]);
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = vec![];

        for _ in 0..10 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();

            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..100 {
                    let v = x_clone.as_ref();
                    assert_eq!(v.iter().sum::<i32>(), 15);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn concurrent_writes() {
        let x = Arw::new(0i32);
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = vec![];

        for _ in 0..10 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();

            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..100 {
                    *x_clone.as_mut() += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(*x.as_ref(), 1000);
    }

    #[test]
    fn concurrent_clone_and_drop() {
        let x = Arw::new(42);
        let barrier = Arc::new(Barrier::new(20));
        let mut handles = vec![];

        for _ in 0..20 {
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

        assert_eq!(Arw::strong_count(&x), 1);
    }

    #[test]
    fn concurrent_upgrade_during_drop() {
        for _ in 0..10 {
            let x = Arw::new(42);
            let weak = x.downgrade();

            let weak_clone = weak.clone();
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    if let Some(strong) = weak_clone.upgrade() {
                        assert_eq!(*strong.as_ref(), 42);
                    }
                }
            });

            // Piccolo delay per dare una chance al thread
            thread::yield_now();
            drop(x);
            handle.join().unwrap();
        }
    }

    #[test]
    fn concurrent_downgrade_upgrade() {
        let x = Arw::new(42);
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = vec![];

        for _ in 0..10 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();

            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..100 {
                    let w = x_clone.downgrade();
                    let _ = w.upgrade();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(Arw::strong_count(&x), 1);
        assert_eq!(Arw::weak_count(&x), 0);
    }

    // ==================== UB Detection ====================

    #[test]
    fn ub_detection_double_free() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
        const CANARY: u64 = 0xDEADBEEF;

        struct UBDetector {
            canary: u64,
        }

        impl Drop for UBDetector {
            fn drop(&mut self) {
                assert_eq!(self.canary, CANARY, "Double-free or corruption!");
                DROP_COUNT.fetch_add(1, SeqCst);
            }
        }

        DROP_COUNT.store(0, SeqCst);

        let x = Arw::new(UBDetector { canary: CANARY });
        let y = x.clone();

        drop(x);
        drop(y);

        assert_eq!(DROP_COUNT.load(SeqCst), 1);
    }

    #[test]
    fn ub_detection_data_race() {
        #[derive(Clone)]
        struct Checker {
            a: i64,
            b: i64,
        }

        impl Checker {
            fn set(&mut self, v: i64) {
                self.a = v;
                self.b = v;
            }
            fn verify(&self) -> bool {
                self.a == self.b
            }
        }

        let x = Arw::new(Checker { a: 0, b: 0 });
        let barrier = Arc::new(Barrier::new(4));
        let errors = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for t in 0..4 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            let errors_clone = errors.clone();

            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..100 {
                    x_clone.as_mut().set((t * 100 + i) as i64);
                    if !x_clone.as_ref().verify() {
                        errors_clone.fetch_add(1, SeqCst);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(errors.load(SeqCst), 0, "Data race detected!");
    }

    #[test]
    fn ub_detection_weak_upgrade_race() {
        for _ in 0..10 {
            let x = Arw::new(42i32);
            let barrier = Arc::new(Barrier::new(3));
            let mut handles = vec![];

            for _ in 0..2 {
                let weak = x.downgrade();
                let barrier_clone = barrier.clone();

                handles.push(thread::spawn(move || {
                    barrier_clone.wait();
                    for _ in 0..100 {
                        if let Some(s) = weak.upgrade() {
                            assert_eq!(*s.as_ref(), 42);
                        }
                    }
                }));
            }

            barrier.wait();
            drop(x);

            for h in handles {
                h.join().unwrap();
            }
        }
    }

    // ==================== Edge Cases ====================

    #[test]
    fn many_clones() {
        let x = Arw::new(42);
        let clones: Vec<_> = (0..1000).map(|_| x.clone()).collect();

        assert_eq!(Arw::strong_count(&x), 1001);
        drop(clones);
        assert_eq!(Arw::strong_count(&x), 1);
    }

    #[test]
    fn many_weak_refs() {
        let x = Arw::new(42);
        let weaks: Vec<_> = (0..1000).map(|_| x.downgrade()).collect();

        assert_eq!(Arw::weak_count(&x), 1000);

        for w in &weaks {
            assert!(w.upgrade().is_some());
        }

        drop(weaks);
        assert_eq!(Arw::weak_count(&x), 0);
    }

    #[test]
    fn recursive_structure() {
        struct Node {
            value: i32,
            next: Option<Arw<Node>>,
        }

        let node3 = Arw::new(Node {
            value: 3,
            next: None,
        });
        let node2 = Arw::new(Node {
            value: 2,
            next: Some(node3),
        });
        let node1 = Arw::new(Node {
            value: 1,
            next: Some(node2),
        });

        assert_eq!(node1.as_ref().value, 1);
        assert_eq!(node1.as_ref().next.as_ref().unwrap().as_ref().value, 2);
    }

    // ==================== Miri-friendly ====================

    #[test]
    fn miri_aliasing() {
        let x = Arw::new(42i32);
        let y = x.clone();

        let v1 = *x.as_ref();
        let v2 = *y.as_ref();
        assert_eq!(v1, v2);

        *x.as_mut() = 100;
        assert_eq!(*y.as_ref(), 100);
    }

    #[test]
    fn miri_stacked_borrows() {
        let x = Arw::new(vec![1, 2, 3]);

        {
            let r = x.as_ref();
            assert_eq!(r.len(), 3);
        }

        {
            let mut w = x.as_mut();
            w.push(4);
        }

        {
            let r = x.as_ref();
            assert_eq!(r.len(), 4);
        }
    }

    #[test]
    fn downgrade_upgrade_cycle() {
        let x = Arw::new(String::from("test"));

        for _ in 0..100 {
            let weak = x.downgrade();
            let upgraded = weak.upgrade().unwrap();
            assert_eq!(*upgraded.as_ref(), "test");
        }

        assert_eq!(Arw::strong_count(&x), 1);
        assert_eq!(Arw::weak_count(&x), 0);
    }

    #[test]
    fn upgrade_keeps_alive() {
        let x = Arw::new(42);
        let weak = x.downgrade();
        let upgraded = weak.upgrade().unwrap();

        drop(x);

        assert_eq!(*upgraded.as_ref(), 42);
        assert_eq!(Arw::strong_count(&upgraded), 1);
    }
}
