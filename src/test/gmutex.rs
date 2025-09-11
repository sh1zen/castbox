
#[cfg(test)]
mod tests_gmutex1 {
    use crate::mutex::{Grutex, Mutex};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn kiko() {
        let m = Grutex::new();

        m.lock_exclusive();

        let mc = m.clone();
        let t1 = thread::spawn(move || {
            mc.lock_group(0);
            mc.lock_group(0);
            mc.lock_group(0);

            thread::sleep(Duration::from_millis(200));
            mc.unlock_all_group(None);
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
    fn stress_test() {
        let mut handles = vec![];

        let mutex = Grutex::new();

        mutex.lock_exclusive();

        for _i in 0..100 {
            let m1 = mutex.clone();
            handles.push(thread::spawn(move || {
                m1.lock_group(0);
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
        use std::thread;
        use std::thread::sleep;
        use std::time::Duration;

        let mutex = Grutex::new();

        let m1 = mutex.clone();
        let m2 = mutex.clone();

        mutex.lock_group(0);
        mutex.lock_group(0);

        mutex.unlock_group(0);

        let h1 = thread::spawn(move || {
            m1.lock_exclusive();
            sleep(Duration::from_millis(100));
            m1.unlock_exclusive();
        });

        let h2 = thread::spawn(move || {
            m2.lock_exclusive();
            m2.unlock_exclusive();
        });

        mutex.unlock_group(0);

        h1.join().unwrap();
        h2.join().unwrap();

        drop(mutex);
    }

    #[test]
    fn refcount_clone_drop() {
        let m = Grutex::new();
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
        let m = Grutex::new();
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
        let m = Grutex::new();

        let entered_group = Arc::new(AtomicBool::new(false));
        let entered_excl = Arc::new(AtomicBool::new(false));

        m.lock_exclusive();
        let eg = entered_group.clone();
        let mg = m.clone();
        let tg = thread::spawn(move || {
            mg.lock_group(0);
            eg.store(true, Ordering::Release);
            mg.unlock_group(0);
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
        let m = Grutex::new();
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
                mm.lock_group(0);
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
        m.unlock_all_group(Some(0));
        assert!(max_concurrent.load(Ordering::Acquire) > 1);
        assert!(!m.is_locked_exclusive());
    }

    #[test]
    fn exclusives_are_mutually_exclusive() {
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
    fn group_batch_then_exclusive() {
        let m = Grutex::new();
        const G: usize = 4;
        let barrier_in = Arc::new(Barrier::new(G));
        let barrier_out = Arc::new(Barrier::new(G));

        let mut tg = Vec::new();
        for _ in 0..G {
            let mm = m.clone();
            let bin = barrier_in.clone();
            let bout = barrier_out.clone();
            tg.push(thread::spawn(move || {
                mm.lock_group(0);
                bin.wait();
                thread::sleep(Duration::from_millis(30));
                bout.wait();
                mm.unlock_group(0);
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
    fn unlock_panics_if_group_locked() {
        let m = Grutex::new();
        m.lock_group(0);
        let res = std::panic::catch_unwind(|| {
            m.unlock_exclusive();
        });
        assert!(res.is_err());
        m.unlock_group(0);
    }

    #[test]
    fn stress_multi_lock() {
        let m = Grutex::new();

        let mut ths = Vec::new();
        for id in 0..8 {
            let mm = m.clone();
            ths.push(thread::spawn(move || {
                for i in 0..100 {
                    if (id + i) % 3 == 0 {
                        mm.lock_exclusive();
                        mm.unlock_exclusive();
                    } else {
                        mm.lock_group(0);
                        mm.unlock_group(0);
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
    }
}




#[cfg(test)]
mod tests_gmutex {
    use crate::mutex::Grutex;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn exclusive_lock_basic() {
        let gm = Arc::new(Grutex::new());
        let gm1 = gm.clone();

        let handle = thread::spawn(move || {
            gm1.lock_exclusive();
            thread::sleep(Duration::from_millis(50));
            gm1.unlock_exclusive();
        });

        thread::sleep(Duration::from_millis(10));
        // deve essere locked esclusivo
        assert!(gm.is_locked_exclusive());
        handle.join().unwrap();
        assert!(!gm.is_locked());
    }

    #[test]
    fn group_lock_basic() {
        let gm = Arc::new(Grutex::new());
        let gm1 = gm.clone();
        let gm2 = gm.clone();

        let h1 = thread::spawn(move || {
            gm1.lock_group(0);
            thread::sleep(Duration::from_millis(50));
            gm1.unlock_group(0);
        });

        let h2 = thread::spawn(move || {
            gm2.lock_group(0);
            thread::sleep(Duration::from_millis(50));
            gm2.unlock_group(0);
        });

        thread::sleep(Duration::from_millis(10));
        // dovrebbe essere locked di gruppo
        assert!(gm.is_locked_group());
        h1.join().unwrap();
        h2.join().unwrap();
        assert!(!gm.is_locked());
    }

    #[test]
    fn exclusive_blocks_group() {
        let gm = Arc::new(Grutex::new());
        let gm1 = gm.clone();
        let gm2 = gm.clone();

        let h_excl = thread::spawn(move || {
            gm1.lock_exclusive();
            thread::sleep(Duration::from_millis(100));
            gm1.unlock_exclusive();
        });

        thread::sleep(Duration::from_millis(10));

        let h_group = thread::spawn(move || {
            // dovrebbe essere sospeso finché l'exclusive non è rilasciato
            gm2.lock_group(1);
            gm2.unlock_group(1);
        });

        thread::sleep(Duration::from_millis(20));
        assert!(gm.is_locked_exclusive());
        h_excl.join().unwrap();
        h_group.join().unwrap();
        assert!(!gm.is_locked());
    }

    #[test]
    fn group_blocks_exclusive() {
        let gm = Arc::new(Grutex::new());
        let gm1 = gm.clone();
        let gm2 = gm.clone();

        let h_group = thread::spawn(move || {
            gm1.lock_group(0);
            thread::sleep(Duration::from_millis(100));
            gm1.unlock_group(0);
        });

        let h_excl = thread::spawn(move || {
            // dovrebbe essere sospeso finché il gruppo non rilascia
            gm2.lock_exclusive();
        });

        h_group.join().unwrap();
        h_excl.join().unwrap();
        assert!(gm.is_locked_exclusive());
    }

    #[test]
    fn unlock_all_group() {
        let gm = Grutex::new();

        gm.lock_group(0);
        gm.lock_group(1);
        assert!(gm.is_locked_group());
        gm.unlock_all_group(None); // sblocca tutti i gruppi
        assert!(!gm.is_locked());
    }

    #[test]
    fn atomic_counters_consistency() {
        let gm = Grutex::new();

        gm.lock_group(0);
        gm.lock_group(1);
        assert_eq!(gm.get_group_locked_for(0), 1);
        assert_eq!(gm.get_group_locked_for(1), 1);
        assert_eq!(gm.get_group_locked(), 2);

        gm.unlock_group(0);
        assert_eq!(gm.get_group_locked_for(0), 0);
        assert_eq!(gm.get_group_locked(), 1);

        gm.unlock_group(1);
        assert_eq!(gm.get_group_locked(), 0);
    }

    #[test]
    fn high_concurrency_test() {
        const THREADS: usize = 100;
        const ITERATIONS: usize = 2;

        let gm = Arc::new(Grutex::new());
        let mut handles = Vec::new();

        for _ in 0..THREADS {
            let gm_clone = gm.clone();
            let handle = thread::spawn(move || {
                for i in 0..ITERATIONS {
                    if i % 5 == 0 {
                        // 20% chance: lock exclusive
                        gm_clone.lock_exclusive();
                        // simula lavoro
                        thread::sleep(Duration::from_millis(10));
                        gm_clone.unlock_exclusive();
                    } else {
                        // 80% chance: lock gruppo casuale
                        let group_id = i % 5;
                        gm_clone.lock_group(group_id);
                        thread::sleep(Duration::from_millis(10));
                        gm_clone.unlock_group(group_id);
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Alla fine, nessun lock attivo
        assert!(!gm.is_locked());
        assert_eq!(gm.get_group_locked(), 0);
    }
}
