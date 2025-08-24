mod tests_atomic_map {
    use std::sync::Arc;
    use std::thread;
    use crate::collections::AtomicHashMap;

    #[test]
    fn stress_test() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;
        let vec = AtomicHashMap::new();
        let vec_c = vec.clone();

        let barrier = Arc::new(Barrier::new(350));

        vec_c.insert(1, "hello".to_string());
        vec_c.insert(2, "world".to_string());
        vec_c.insert(3, "test".to_string());
        assert_eq!(vec.remove(&1).unwrap(), "hello");
        assert_eq!(vec.remove(&2).unwrap(), "world");
        assert_eq!(vec.remove(&3).unwrap(), "test");
        assert!(vec.remove(&1).is_none());

        let mut handles = vec![];

        for i in 0..100 {
            let vec = vec.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                vec.insert(i, "hello".to_string());
            }));
        }

        for i in 1..101 {
            let vec = vec.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                vec.insert(i, "hello".to_string());
            }));
        }

        for i in 0..50 {
            let vec = vec.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                vec.remove(&i);
            }));
        }

        for i in 0..50 {
            let vec = vec.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                if let Some(mut t) = vec.get_mut(&i) {
                    t.push_str(":1");
                }
            }));
        }

        for _ in 0..50 {
            let vec = vec.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in vec.iter() {}
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(vec.len() > 50, true);
    }

    #[test]
    fn insert_and_get() {
        let map = AtomicHashMap::new();
        map.insert("key1", 10);
        map.insert("key2", 20);

        assert_eq!(*map.get("key1").unwrap(), 10);
        assert_eq!(*map.get("key2").unwrap(), 20);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn overwrite_value() {
        let map = AtomicHashMap::new();
        map.insert("key", 42);
        assert_eq!(*map.get("key").unwrap(), 42);

        map.insert("key", 99);
        assert_eq!(*map.get("key").unwrap(), 99);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn remove_key() {
        let map = AtomicHashMap::new();
        map.insert("to_remove", 123);
        assert_eq!(map.len(), 1);

        let removed = map.remove("to_remove");
        assert_eq!(removed, Some(123));
        assert!(map.get("to_remove").is_none());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn get_mut_changes_value() {
        let map = AtomicHashMap::new();
        map.insert("x", 5);

        {
            let mut val = map.get_mut("x").unwrap();
            *val = 77;
        }

        assert_eq!(*map.get("x").unwrap(), 77);
    }

    #[test]
    fn iterates_all_items() {
        let map = AtomicHashMap::with_capacity(8);
        for i in 0..10 {
            map.insert(i, i * 10);
        }

        let mut seen = 0;
        for (k, v) in map.iter() {
            assert_eq!(*v, *k * 10);
            seen += 1;
        }
        assert_eq!(seen, 10);
    }

    #[test]
    fn clone_and_drop() {
        let map = AtomicHashMap::new();
        map.insert("k", 1);

        {
            let map2 = map.clone();
            assert_eq!(*map2.get("k").unwrap(), 1);
        } // map2 dropped, ref_count decremented

        assert_eq!(*map.get("k").unwrap(), 1);
    }

    #[test]
    fn concurrent_inserts_and_reads() {
        let map = Arc::new(AtomicHashMap::new());

        let mut handles = vec![];

        for i in 0..4 {
            let map_clone = Arc::clone(&map);
            handles.push(thread::spawn(move || {
                for j in 0..50 {
                    let key = format!("k{}_{}", i, j);
                    map_clone.insert(key.clone(), j);
                    assert_eq!(*map_clone.get(&key).unwrap(), j);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // global check
        let total = map.len();
        assert_eq!(total, 4 * 50);
    }
}
