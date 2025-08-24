mod tests_arw {
    use crate::mutex::WatchGuardRef;
    use crate::{Arw, WeakArw};
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    use std::sync::Barrier;
    use std::time::Instant;
    use std::{hint, thread};

    #[test]
    fn stress_test() {
        let started = Instant::now();
        let a = Arw::new("hello".to_string());

        assert!(Arw::is_unique(&a));

        let mut handles = vec![];

        let val = Arw::clone(&a);

        for _ in 0..100 {
            let val_clone = val.clone();

            handles.push(thread::spawn(move || {
                let mut rc = val_clone.as_mut();
                // add some dirty
                for _ in 0..100 {
                    rc.push_str(":1")
                }
            }));
        }

        for _ in 0..100 {
            let val_clone = val.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _clone = val_clone.as_ref();
                    let _u = hint::black_box(_clone);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(val.as_ref().split(":").count(), 10_001);
        println!("{:?}", started.elapsed());
    }

    #[test]
    fn test_map() {
        let x = Arw::new(5i32);
        assert_eq!(
            *x.map(|x: WatchGuardRef<'_, i32>| (*x * 2) as u64).as_ref(),
            10u64
        );
    }

    #[test]
    fn test_strong_weak_counts() {
        let x = Arw::new("hello");
        let y = x.clone();
        {
            let j = y.clone();
            assert_eq!(Arw::weak_count(&x), 0);
            assert_eq!(Arw::strong_count(&j), 3);
        }
        let x_d = x.downgrade();
        let weak_clone = x_d.clone();
        assert_eq!(Arw::strong_count(&x), 2);
        assert_eq!(Arw::weak_count(&x), 2);

        drop(x);
        let x = weak_clone.upgrade().unwrap();
        drop(weak_clone);
        assert_eq!(Arw::strong_count(&x), 2);
        assert_eq!(Arw::weak_count(&x), 1);
    }

    #[test]
    fn test_downgrade_and_upgrade() {
        let x = Arw::new("test");
        let weak = x.downgrade();

        assert!(weak.upgrade().is_some());
        assert!(weak.upgrade().unwrap().as_ref().eq(&"test"));
    }

    #[test]
    fn test_weak_drops_when_no_strong() {
        let weak: WeakArw<i32>;
        {
            let x = Arw::new(42);
            weak = x.downgrade();
            assert!(weak.upgrade().is_some());
        }
        assert_eq!(weak.clone().strong_count(), 0);

        let _x = weak.clone();
        assert_eq!(weak.clone().weak_count(), 3);

        // After drop, weak cannot upgrade
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn test_default_fill() {
        let x: Arw<i32> = Default::default();
        let x = Arw::fill(x, 10i32);
        assert_eq!(x.as_ref(), 10i32);

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

        let x: Arw<Def> = Arw::default();
        assert_eq!(x.as_ref().data, String::from("hello"));
    }

    #[test]
    fn test_from_raw_in_reconstruction() {
        let x = Arw::new(String::from("hello"));
        let raw = Arw::into_raw(x);

        let y = unsafe { Arw::from_raw(raw) };
        assert_eq!(y.as_ref(), "hello");
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
        let foo = Arw::new(Foo);
        {
            let _x = Arw::clone(&foo);
            let _weak_foo = Arw::downgrade(&foo);
        }
        let weak_foo = Arw::downgrade(&foo);
        let other_weak_foo = WeakArw::clone(&weak_foo);

        drop(weak_foo); // Doesn't do anything
        drop(foo); // drop data here

        assert!(other_weak_foo.upgrade().is_none());
        assert_eq!(DROP_COUNTER.load(Acquire), 1);
    }

    #[test]
    fn test_concurrent_clone_and_drop() {
        let x = Arw::new(100i32);
        let mut handles = vec![];
        let barrier = Arw::new(Barrier::new(300));

        for i in 0..300 {
            let x_clone = x.clone();
            let barrier_clone = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier_clone.as_ref().wait();
                assert_eq!(x_clone.as_ref(), 100);
            }));

            assert_eq!(Arw::strong_count(&x), i + 2);
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(Arw::strong_count(&x), 1);
        assert_eq!(Arw::weak_count(&x), 0);
    }

    #[test]
    fn test_concurrent_downgrade_and_upgrade() {
        let x = Arw::new("abc");
        let weak = x.downgrade();
        let mut handles = vec![];

        for _ in 0..10 {
            let weak_clone = weak.clone();
            handles.push(thread::spawn(move || {
                let upgraded = weak_clone.upgrade();
                if let Some(v) = upgraded {
                    assert_eq!(v.as_ref(), "abc");
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
        let x = Arw::new(String::from("hello"));
        let y = x.clone();

        x.as_mut().push_str(":1");
        y.as_mut().push_str(":2");

        assert_eq!(y.as_ref(), "hello:1:2");

        let weak = x.downgrade();
        let mut handles = vec![];

        for i in 0..10 {
            let a_clone = Arw::clone(&x);
            handles.push(thread::spawn(move || {
                let mut val = a_clone.as_mut();
                val.push_str(format!(":{}", i).as_str());
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(weak.upgrade().unwrap().as_ref().split(":").count(), 13);
    }

    #[test]
    fn test_threaded_upgrade_after_drop() {
        let weak_holder: WeakArw<&str>;
        {
            let x = Arw::new("persistent");
            weak_holder = x.downgrade();

            let x2 = x.clone();

            thread::spawn(move || {
                let val = x2.as_ref();
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
        let x = Arw::new(3i32);
        if let Ok(t) = Arw::try_unwrap(x) {
            assert_eq!(t, 3i32);
        }

        let x = Arw::new(4);
        let _y = Arw::clone(&x);
        assert_eq!(*Arw::try_unwrap(x).unwrap_err().as_ref(), 4);
    }
}
