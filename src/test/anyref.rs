mod tests_any_ref {
    use crossync::sync::WatchGuardRef;
    use crate::anyref::{AnyRef, WeakAnyRef};
    use std::any::TypeId;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    use std::thread;

    #[test]
    fn stress_test_any_ref() {
        let a = AnyRef::new("hello".to_string());

        if let Some(s) = a.try_downcast_ref::<String>() {
            assert_eq!(s, "hello");
            drop(s);
        }



        if let Some(mut s) = a.try_downcast_mut::<String>() {
            *s = "hello:1".to_string();
            drop(s);
            assert_eq!(*a.as_ref::<String>(), "hello:1");
        }

        let mut handles = vec![];

        let val = AnyRef::clone(&a);

        for _ in 0..100 {
            let val_clone = val.clone();

            handles.push(thread::spawn(move || {
                let rc = val_clone.as_mut::<String>();
                // add some dirty
                let mut rc = std::hint::black_box(rc);
                for _ in 0..100 {
                    rc.push_str(":1")
                }
            }));
        }

        for _ in 0..100 {
            let val_clone = val.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _clone = val_clone.as_ref::<String>();
                    let _ = std::hint::black_box(&_clone);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(val.as_ref::<String>().split(":").count(), 10_002);
    }

    #[test]
    fn test_map() {
        let x = AnyRef::new(5i32);
        assert_eq!(
            *x.map(|x: WatchGuardRef<'_, i32>| (*x * 2) as u64)
                .as_ref::<u64>(),
            10u64
        );
    }

    #[test]
    fn new_and_type() {
        let x = AnyRef::new(42u32);
        assert_eq!(x.inner().type_id, TypeId::of::<u32>());
        assert_eq!(
            AnyRef::new(String::new()).inner().type_id,
            TypeId::of::<String>()
        );
        assert_eq!(
            AnyRef::new(Box::new(String::new())).inner().type_id,
            TypeId::of::<Box<String>>()
        );
    }

    #[test]
    fn test_strong_weak_counts() {
        let x = AnyRef::new("hello");
        let y = x.clone();
        {
            let j = y.clone();
            assert_eq!(AnyRef::weak_count(&x), 0);
            assert_eq!(AnyRef::strong_count(&j), 3);
        }
        let x_d = x.downgrade();
        let weak_clone = x_d.clone();
        assert_eq!(AnyRef::strong_count(&x), 2);
        assert_eq!(AnyRef::weak_count(&x), 2);

        drop(x);
        let x = weak_clone.upgrade().unwrap();
        drop(weak_clone);
        assert_eq!(AnyRef::strong_count(&x), 2);
        assert_eq!(AnyRef::weak_count(&x), 1);
    }

    #[test]
    fn test_downgrade_and_upgrade() {
        let x = AnyRef::new("test");
        let weak = x.downgrade();

        assert!(weak.upgrade().is_some());
        assert!(weak.upgrade().unwrap().as_ref::<&str>().eq(&"test"));
    }

    #[test]
    fn test_downcast_success() {
        let x = AnyRef::new(1234i64);
        let val = x.as_ref::<i64>();
        assert_eq!(*val, 1234);
    }

    #[test]
    fn test_downcast_fail() {
        let x = AnyRef::new(1234i64);
        let r = x.try_downcast_ref::<u32>();
        assert!(r.is_none())
    }

    #[test]
    fn test_try_downcast_mut_success() {
        let x = AnyRef::new(42);
        let clone = x.clone().downgrade();
        let y = x.try_downcast_mut::<i32>();
        assert!(y.is_some());
        if let Some(mut x) = y {
            *x += 1;
            assert_eq!(*x, 43);
        };

        assert_eq!(clone.upgrade().unwrap().as_ref::<i32>(), 43);
    }

    #[test]
    fn test_weak_drops_when_no_strong() {
        let weak: WeakAnyRef;
        {
            let x = AnyRef::new(42);
            weak = x.downgrade();
            assert!(weak.upgrade().is_some());
        }
        assert_eq!(weak.clone().strong_count(), 0);

        let _x = weak.clone();
        assert_eq!(weak.clone().weak_count(), 2);

        // After drop, weak cannot upgrade
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn test_default_fill() {
        let x: AnyRef = Default::default();
        let x = AnyRef::fill(x, 10i32);
        assert_eq!(x.as_ref::<i32>().clone(), 10i32);

        struct Def {
            data: String,
        }

        impl Default for Def {
            fn default() -> Self {
                Def {
                    data: String::from("hello"),
                }
            }
        }

        let x = AnyRef::default_with::<Def>();
        assert_eq!(x.as_ref::<Def>().data, String::from("hello"));
    }

    #[test]
    fn test_from_raw_in_reconstruction() {
        let x = AnyRef::new(String::from("hello"));
        let raw = AnyRef::into_raw(x);

        let y = unsafe { AnyRef::from_raw(raw) };
        let val = y.as_ref::<String>();
        assert_eq!(val, "hello");
    }

    #[test]
    fn test_drop() {
        struct Foo;
        static DROP_COUNTER: AtomicU8 = AtomicU8::new(0);
        impl Drop for Foo {
            fn drop(&mut self) {
                DROP_COUNTER.fetch_add(1, Relaxed);
            }
        }
        let foo = AnyRef::new(Foo);
        {
            let _x = AnyRef::clone(&foo);
            let _weak_foo = AnyRef::downgrade(&foo);
        }
        let weak_foo = AnyRef::downgrade(&foo);
        let other_weak_foo = WeakAnyRef::clone(&weak_foo);

        drop(weak_foo); // Doesn't do anything
        drop(foo); // drop data here

        assert!(other_weak_foo.upgrade().is_none());
        assert_eq!(DROP_COUNTER.load(Acquire), 1);
    }

    #[test]
    fn test_concurrent_clone_and_drop() {
        let x = AnyRef::new(100i32);
        let mut handles = vec![];
        let barrier = AnyRef::new(Barrier::new(300));

        for i in 0..300 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier_clone.as_ref::<Barrier>().wait();
                let val = x_clone.as_ref::<i32>();
                assert_eq!(*val, 100);
            }));

            assert_eq!(AnyRef::strong_count(&x), i + 2);
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(AnyRef::strong_count(&x), 1);
        assert_eq!(AnyRef::weak_count(&x), 0);
    }

    #[test]
    fn test_concurrent_downgrade_and_upgrade() {
        let x = AnyRef::new("abc");
        let weak = x.downgrade();
        let mut handles = vec![];

        for _ in 0..10 {
            let weak_clone = weak.clone();
            handles.push(thread::spawn(move || {
                let upgraded = weak_clone.upgrade();
                if let Some(v) = upgraded {
                    assert_eq!(v.as_ref::<&str>(), "abc");
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(weak.upgrade().is_some());
    }

    #[test]
    fn test_thread_safe() {
        let x = AnyRef::new(String::from("hello"));
        let y = x.clone();

        if let Some(mut v) = x.try_downcast_mut::<String>() {
            v.push_str(":1");
        }

        if let Some(mut v) = y.try_downcast_mut::<String>() {
            v.push_str(":2");
        }

        assert_eq!(*y.as_ref::<String>(), "hello:1:2");

        let weak = x.downgrade();
        let mut handles = vec![];

        for i in 0..10 {
            let a_clone = AnyRef::clone(&x);
            handles.push(thread::spawn(move || {
                let mut val = a_clone.as_mut::<String>();
                val.push_str(format!(":{}", i).as_str());
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            weak.upgrade()
                .unwrap()
                .as_ref::<String>()
                .split(":")
                .count(),
            13
        );
    }

    #[test]
    fn test_threaded_upgrade_after_drop() {
        let weak_holder: WeakAnyRef;
        {
            let x = AnyRef::new("persistent");
            weak_holder = x.downgrade();

            let x2 = x.clone();

            thread::spawn(move || {
                let val = x2.as_ref::<&str>();
                assert_eq!(*val, "persistent");
                // Drop happens when thread ends
            })
            .join()
            .unwrap();
        }
        // All strong refs dropped, weak is now invalid
        assert!(weak_holder.upgrade().is_none());
    }

    #[test]
    fn test_try_unwrap() {
        let x = AnyRef::new(314i32);
        if let Ok(t) = AnyRef::try_unwrap::<i32>(x) {
            assert_eq!(t, 314i32);
        }

        let x = AnyRef::new(4);
        let _y = AnyRef::clone(&x);
        assert_eq!(
            *AnyRef::try_unwrap::<i32>(x).unwrap_err().as_ref::<i32>(),
            4
        );
    }
}
