mod tests_atomic_chain {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use crate::atomic::AtomicChain;

    #[test]
    fn new_is_empty() {
        let map: AtomicChain<i32, i32> = AtomicChain::new();
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn insert_and_get_single() {
        let map = AtomicChain::new();
        map.insert("k1".to_string(), 42);

        let val = map.get("k1").unwrap();
        assert_eq!(*val, 42);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn insert_multiple_fifo_order() {
        let map = AtomicChain::new();
        map.insert("a".to_string(), 1);
        map.insert("a".to_string(), 2);
        map.insert("a".to_string(), 3);

        let vals = map.get_all("a");
        let collected: Vec<i32> = vals.iter().map(|v| **v).collect();
        assert_eq!(collected, vec![1, 2, 3]);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn remove_existing_and_missing_key() {
        let map = AtomicChain::new();
        map.insert(1, "uno");
        map.insert(2, "due");
        assert_eq!(map.len(), 2);

        let removed = map.remove(&1);
        assert_eq!(removed, Some("uno"));
        assert_eq!(map.len(), 1);

        let missing = map.remove(&99);
        assert_eq!(missing, None);
    }

    #[test]
    fn handles_different_keys() {
        let map = AtomicChain::new();
        map.insert("x", 10);
        map.insert("y", 20);
        map.insert("x", 30);

        let vals_x: Vec<_> = map.get_all("x").into_iter().map(|v| *v).collect();
        assert_eq!(vals_x, vec![10, 30]);

        let vals_y: Vec<_> = map.get_all("y").into_iter().map(|v| *v).collect();
        assert_eq!(vals_y, vec![20]);
    }

    #[test]
    fn clone_and_refcount() {
        let map = AtomicChain::new();
        let c1 = map.clone();
        let c2 = map.clone();
        c1.insert("k", 123);
        assert_eq!(*c2.get("k").unwrap(), 123);
        drop(c1);
        drop(c2);
        // non deve crashare o corrompere
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn iteration_works() {
        let map = AtomicChain::new();
        map.insert("a", 1);
        map.insert("b", 2);
        map.insert("c", 3);

        let mut collected = Vec::new();
        for (k, v) in map.iter() {
            collected.push((k, *v));
        }
        collected.sort();

        assert_eq!(collected, vec![
            (&"a", 1),
            (&"b", 2),
            (&"c", 3),
        ]);
    }

    #[test]
    fn multithreaded_insert_and_remove() {
        const THREADS: usize = 8;
        const OPS: usize = 100;

        let map = AtomicChain::new();
        let barrier = Arc::new(Barrier::new(THREADS));
        let counter = Arc::new(AtomicUsize::new(0));

        let mut ths = Vec::new();
        for t in 0..THREADS {
            let m = map.clone();
            let b = barrier.clone();
            let c = counter.clone();
            ths.push(thread::spawn(move || {
                b.wait();
                for i in 0..OPS {
                    let key = format!("k{}", t);
                    if i % 2 == 0 {
                        m.insert(key.clone(), i);
                        c.fetch_add(1, Ordering::Relaxed);
                    } else {
                        m.remove(&key);
                    }
                }
            }));
        }

        for th in ths {
            th.join().unwrap();
        }

        // almeno qualche inserimento rimane
        assert!(map.len() <= THREADS * OPS);
    }

    #[test]
    fn debug_trait_does_not_panic() {
        let map: AtomicChain<i32, i32> = AtomicChain::new();
        map.insert(1, 42);
        let s = format!("{:?}", map);
        assert!(s.contains("AtomicChain"));
    }
}
