mod tests_atomic_vec {
    use std::sync::{Arc, Barrier};
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    use std::thread;
    use crate::collections::AtomicVec;

    #[test]
    fn stress_test() {
        use std::thread;
        let vec = AtomicVec::new();
        let vec_c = vec.clone();

        vec_c.push(10);
        vec.push(20);
        vec_c.push(30);
        assert_eq!(vec.pop().unwrap(), 10);
        assert_eq!(vec.pop().unwrap(), 20);
        assert_eq!(vec.pop().unwrap(), 30);

        let mut handles = vec![];

        for _ in 0..100 {
            let vec_c = vec.clone();
            handles.push(thread::spawn(move || {
                vec_c.push(10);
            }));

            let vec_c = vec.clone();
            handles.push(thread::spawn(move || {
                vec_c.pop();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        for _ in 0..50 {
            vec_c.pop();
        }

        assert!(vec.pop().is_none());
    }

    #[test]
    fn new_is_empty() {
        let v: AtomicVec<i32> = AtomicVec::new();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn push_and_pop_single() {
        let v = AtomicVec::new();
        v.push(10);
        assert_eq!(v.len(), 1);
        assert_eq!(v.pop(), Some(10));
        assert!(v.is_empty());
        assert_eq!(v.pop(), None);
    }

    #[test]
    fn fifo_order() {
        let v = AtomicVec::new();
        v.push(1);
        v.push(2);
        v.push(3);
        assert_eq!(v.pop(), Some(1));
        assert_eq!(v.pop(), Some(2));
        assert_eq!(v.pop(), Some(3));
        assert_eq!(v.pop(), None);
    }

    #[test]
    fn clone_and_refcount() {
        let v = AtomicVec::new();
        let c1 = v.clone();
        let c2 = v.clone();
        assert_eq!(v.len(), 0);
        drop(c1);
        drop(c2);
        // qui non possiamo ispezionare direttamente ref_count,
        // ma il Drop non deve corrompere la memoria.
        v.push(42);
        assert_eq!(v.pop(), Some(42));
    }

    #[test]
    fn multithreaded_push() {
        let v = Arc::new(AtomicVec::new());
        const N: usize = 8;
        const PER_THREAD: usize = 100;

        let barrier = Arc::new(Barrier::new(N));
        let mut ths = Vec::new();
        for t in 0..N {
            let vv = v.clone();
            let b = barrier.clone();
            ths.push(thread::spawn(move || {
                b.wait();
                for i in 0..PER_THREAD {
                    vv.push(t * 1000 + i);
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        assert_eq!(v.len(), N * PER_THREAD);
    }

    #[test]
    fn multithreaded_pop() {
        let v = Arc::new(AtomicVec::new());
        const N: usize = 4;
        const PER_THREAD: usize = 50;

        for i in 0..(N * PER_THREAD) {
            v.push(i as i32);
        }
        assert_eq!(v.len(), N * PER_THREAD);

        let sum = Arc::new(AtomicIsize::new(0));
        let barrier = Arc::new(Barrier::new(N));
        let mut ths = Vec::new();
        for _ in 0..N {
            let vv = v.clone();
            let ss = sum.clone();
            let b = barrier.clone();
            ths.push(thread::spawn(move || {
                b.wait();
                loop {
                    if let Some(x) = vv.pop() {
                        ss.fetch_add(x as isize, Ordering::AcqRel);
                    } else {
                        break;
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }
        assert!(v.is_empty());

        // somma aritmetica dei primi N*PER_THREAD numeri
        let expected = ((N * PER_THREAD - 1) * (N * PER_THREAD) / 2) as isize;
        assert_eq!(sum.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn stress_push_pop_mixed() {
        let v = Arc::new(AtomicVec::new());
        const THREADS: usize = 6;
        const OPS: usize = 200;

        let barrier = Arc::new(Barrier::new(THREADS));
        let total_pushes = Arc::new(AtomicUsize::new(0));
        let total_pops   = Arc::new(AtomicUsize::new(0));

        let mut ths = Vec::new();
        for id in 0..THREADS {
            let vv = v.clone();
            let b = barrier.clone();
            let pushes = total_pushes.clone();
            let pops = total_pops.clone();
            ths.push(thread::spawn(move || {
                b.wait();
                for i in 0..OPS {
                    if (id + i) % 2 == 0 {
                        vv.push(id * 10000 + i);
                        pushes.fetch_add(1, Ordering::Relaxed);
                    } else {
                        if vv.pop().is_some() {
                            pops.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for t in ths {
            t.join().unwrap();
        }

        let pushes = total_pushes.load(Ordering::Relaxed);
        let pops = total_pops.load(Ordering::Relaxed);
        assert!(pushes >= pops);
        assert_eq!(v.len(), pushes - pops);
    }

    #[test]
    fn debug_trait_does_not_panic() {
        let v: AtomicVec<String> = AtomicVec::new();
        v.push("ciao".to_string());
        let s = format!("{:?}", v);
        assert!(s.contains("AtomicVec"));
    }
}
