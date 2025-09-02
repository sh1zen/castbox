mod tests_mutex {
    use crate::mutex::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn stress_test() {
        let mut handles = vec![];

        let mutex = Mutex::new();

        mutex.lock_exclusive();

        for _i in 0..100 {
            let m1 = mutex.clone();
            handles.push(thread::spawn(move || {
                m1.lock_group();
            }));
        }

        assert!(!mutex.is_locked_group());

        mutex.unlock_exclusive();

        for h in handles {
            h.join().unwrap();
        }

        assert!(mutex.is_locked_group());
        drop(mutex);
    }

    #[test]
    fn test_mutex() {
        use crate::mutex::Mutex;
        use std::thread;
        use std::thread::sleep;
        use std::time::Duration;

        let mutex = Mutex::new();

        let m1 = mutex.clone();
        let m2 = mutex.clone();

        mutex.lock_group();
        mutex.lock_group();

        mutex.unlock_group();

        let h1 = thread::spawn(move || {
            m1.lock_exclusive();
            sleep(Duration::from_millis(100));
            m1.unlock_exclusive();
        });

        let h2 = thread::spawn(move || {
            m2.lock_exclusive();
            m2.unlock_exclusive();
        });

        mutex.unlock_group();

        h1.join().unwrap();
        h2.join().unwrap();

        drop(mutex);
    }

    #[test]
    fn refcount_clone_drop() {
        let m = Mutex::new();
        assert_eq!(m.get_ref_count(), 1);
        let c1 = m.clone();
        let c2 = m.clone();
        assert_eq!(m.get_ref_count(), 3);
        drop(c1);
        drop(c2);
        assert_eq!(m.get_ref_count(), 1);
    }

    #[test]
    fn is_locked_reflects_state() {
        let m = Mutex::new();
        assert!(!m.is_locked_exclusive());
        {
            let _g = m.lock_exclusive();
            assert!(m.is_locked_exclusive());
            m.unlock_exclusive();
        }
        assert!(!m.is_locked_exclusive());
    }

    #[test]
    fn exclusive_blocks_others() {
        let m = Mutex::new();

        let entered_group = Arc::new(AtomicBool::new(false));
        let entered_excl = Arc::new(AtomicBool::new(false));

        m.lock_exclusive();
        let eg = entered_group.clone();
        let mg = m.clone();
        let tg = thread::spawn(move || {
            mg.lock_group();
            eg.store(true, Ordering::Release);
            mg.unlock_group();
        });

        let ee = entered_excl.clone();
        let me = m.clone();
        let te = thread::spawn(move || {
            me.lock_exclusive();
            ee.store(true, Ordering::Release);
            me.unlock_exclusive();
        });

        thread::sleep(Duration::from_millis(50));
        assert!(!entered_group.load(Ordering::Acquire));
        assert!(!entered_excl.load(Ordering::Acquire));

        m.unlock_exclusive();

        tg.join().unwrap();
        te.join().unwrap();

        assert!(entered_group.load(Ordering::Acquire));
        assert!(entered_excl.load(Ordering::Acquire));
    }

    #[test]
    fn group_allows_concurrency() {
        let m = Mutex::new();
        const N: usize = 6;

        let barrier = Arc::new(Barrier::new(N));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut ths = Vec::new();
        for _ in 0..N {
            let mm = m.clone();
            let b = barrier.clone();
            let cur = concurrent.clone();
            let maxc = max_concurrent.clone();
            ths.push(thread::spawn(move || {
                mm.lock_group();
                b.wait();
                let now = cur.fetch_add(1, Ordering::AcqRel) + 1;
                maxc.fetch_max(now, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(20));
                cur.fetch_sub(1, Ordering::AcqRel);
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        m.unlock_all_group();
        assert!(max_concurrent.load(Ordering::Acquire) > 1);
        assert!(!m.is_locked_exclusive());
    }

    #[test]
    fn exclusives_are_mutually_exclusive() {
        let m = Mutex::new();
        let inside = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(true));

        let mut ths = Vec::new();
        for _ in 0..4 {
            let mm = m.clone();
            let inside = inside.clone();
            let ok = ok.clone();
            ths.push(thread::spawn(move || {
                for _ in 0..50 {
                    mm.lock_exclusive();
                    if inside.swap(true, Ordering::AcqRel) {
                        ok.store(false, Ordering::Release);
                    }
                    thread::sleep(Duration::from_millis(1));
                    inside.store(false, Ordering::Release);
                    mm.unlock_exclusive();
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        assert!(ok.load(Ordering::Acquire));
    }

    #[test]
    fn group_batch_then_exclusive() {
        let m = Mutex::new();
        const G: usize = 4;
        let barrier_in = Arc::new(Barrier::new(G));
        let barrier_out = Arc::new(Barrier::new(G));

        let mut tg = Vec::new();
        for _ in 0..G {
            let mm = m.clone();
            let bin = barrier_in.clone();
            let bout = barrier_out.clone();
            tg.push(thread::spawn(move || {
                mm.lock_group();
                bin.wait();
                thread::sleep(Duration::from_millis(30));
                bout.wait();
                mm.unlock_group();
            }));
        }

        let entered_excl = Arc::new(AtomicBool::new(false));
        let ee = entered_excl.clone();
        let me = m.clone();
        let te = thread::spawn(move || {
            me.lock_exclusive();
            ee.store(true, Ordering::Release);
            me.unlock_exclusive();
        });

        te.join().unwrap();
        for t in tg {
            t.join().unwrap();
        }

        assert!(entered_excl.load(Ordering::Acquire));
    }

    #[test]
    #[should_panic(expected = "Trying to unlock a non Locked Group")]
    fn unlock_group_panics_if_not_group() {
        let m = Mutex::new();
        m.lock_exclusive();
        m.unlock_group();
    }

    #[test]
    #[should_panic(expected = "Is not Locked or is a Locked Group")]
    fn unlock_panics_if_not_locked() {
        let m = Mutex::new();
        m.unlock_exclusive();
    }

    #[test]
    fn unlock_panics_if_group_locked() {
        let m = Mutex::new();
        m.lock_group();
        let res = std::panic::catch_unwind(|| {
            m.unlock_exclusive();
        });
        assert!(res.is_err());
        m.unlock_group();
    }

    #[test]
    fn stress_multi_lock() {
        let m = Mutex::new();
        let excl_sum = Arc::new(AtomicI32::new(0));
        let group_entries = Arc::new(AtomicUsize::new(0));

        let mut ths = Vec::new();
        for id in 0..8 {
            let mm = m.clone();
            let excl = excl_sum.clone();
            let ge = group_entries.clone();
            ths.push(thread::spawn(move || {
                for i in 0..100 {
                    if (id + i) % 3 == 0 {
                        mm.lock_exclusive();
                        excl.fetch_add(1, Ordering::Relaxed);
                        mm.unlock_exclusive();
                    } else {
                        mm.lock_group();
                        ge.fetch_add(1, Ordering::Relaxed);
                        thread::sleep(Duration::from_millis(1));
                        mm.unlock_group();
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        assert!(excl_sum.load(Ordering::Relaxed) > 0);
        assert!(group_entries.load(Ordering::Relaxed) > 0);
    }
}
