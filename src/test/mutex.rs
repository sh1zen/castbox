use std::time::Instant;

#[cfg(test)]
mod tests_mutex {
    use crate::mutex::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    pub(crate) fn kiko() {
        let m = Mutex::new();

        m.lock_exclusive();

        let mc = m.clone();
        let t1 = thread::spawn(move || {
            mc.lock_group();
            mc.lock_group();
            mc.lock_group();

            thread::sleep(Duration::from_millis(200));
            mc.unlock_all_group();
        });

        let mc = m.clone();
        let t2 = thread::spawn(move || {
            let _x = mc.clone();
            mc.unlock_exclusive();
            thread::sleep(Duration::from_millis(100));
            mc.lock_exclusive();
        });

        t1.join().unwrap();
        t2.join().unwrap();

        m.unlock_exclusive();
    }

    #[test]
    pub(crate) fn stress_test() {
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
    pub(crate) fn test_mutex() {
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
    pub(crate) fn refcount_clone_drop() {
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
    pub(crate) fn is_locked_reflects_state() {
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
    pub(crate) fn exclusive_blocks_others() {
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
    pub(crate) fn group_allows_concurrency() {
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
    pub(crate) fn exclusives_are_mutually_exclusive() {
        let m = Mutex::new();
        let inside = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(true));

        let mut ths = Vec::new();
        for _i in 0..4 {
            let mm = m.clone();
            let inside = inside.clone();
            let ok = ok.clone();
            ths.push(thread::spawn(move || {
                let _x = _i;
                let _k = mm.get_ref_count();
                for _j in 0..50 {
                    mm.lock_exclusive();
                    if inside.swap(true, Ordering::AcqRel) {
                        ok.store(false, Ordering::Release);
                    }
                    //thread::sleep(Duration::from_millis(1));
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
    pub(crate) fn group_batch_then_exclusive() {
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
    pub(crate) fn unlock_panics_if_group_locked() {
        let m = Mutex::new();
        m.lock_group();
        let res = std::panic::catch_unwind(|| {
            m.unlock_exclusive();
        });
        assert!(res.is_err());
        m.unlock_group();
    }

    #[test]
    pub(crate) fn stress_multi_lock() {
        let m = Mutex::new();

        let mut ths = Vec::new();
        for id in 0..8 {
            let mm = m.clone();
            ths.push(thread::spawn(move || {
                for i in 0..100 {
                    if (id + i) % 3 == 0 {
                        mm.lock_exclusive();
                        mm.unlock_exclusive();
                    } else {
                        mm.lock_group();
                        mm.unlock_group();
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
    }
}

#[test]
fn run_all_tests1() {
    let start = Instant::now();

    tests_mutex::kiko();
    tests_mutex::stress_test();
    // tests_mutex::test_mutex();
    tests_mutex::refcount_clone_drop();
    tests_mutex::is_locked_reflects_state();
    tests_mutex::exclusive_blocks_others();
    tests_mutex::group_allows_concurrency();
    tests_mutex::exclusives_are_mutually_exclusive();
    tests_mutex::group_batch_then_exclusive();
    tests_mutex::unlock_panics_if_group_locked();
    tests_mutex::stress_multi_lock();

    let elapsed = start.elapsed();
    println!("Tempo totale esecuzione test: {:?}", elapsed);
}
