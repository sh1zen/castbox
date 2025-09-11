#[cfg(test)]
mod tests_array {
    use crate::atomic::AtomicArray;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn stress_test() {
        let arr = Arc::new(AtomicArray::with_capacity(200));
        let arr_c = arr.clone();

        // Test sequenziale iniziale
        arr_c.push(10).unwrap();
        arr.push(20).unwrap();
        arr_c.push(30).unwrap();

        assert_eq!(*arr.get(0).unwrap(), 10);
        assert_eq!(*arr.get(1).unwrap(), 20);
        assert_eq!(*arr.get(2).unwrap(), 30);

        let mut handles = vec![];

        for i in 0..100 {
            let arr_c = arr.clone();
            handles.push(thread::spawn(move || {
                let _ = arr_c.push(i); // Ignora errore se pieno
            }));

            let arr_c = arr.clone();
            handles.push(thread::spawn(move || {
                let _ = arr_c.get(i); // Lettura concorrente
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Lettura finale di controllo
        for i in 0..arr.len() {
            if arr_c.get(i).is_none() {
                panic!("len error");
            }
        }

        assert_eq!(arr.len(), 103);
    }

    #[test]
    fn test_creation_and_len() {
        let arr: AtomicArray<i32> = AtomicArray::new();
        assert_eq!(arr.len(), 0);
        assert_eq!(arr.capacity(), arr.capacity());
        assert!(arr.is_empty());
    }

    #[test]
    fn test_push_and_get() {
        let arr = AtomicArray::new();
        assert!(arr.push(42).is_ok());
        assert_eq!(arr.len(), 1);

        let val = arr.get(0).unwrap();
        assert_eq!(*val, 42);
    }

    #[test]
    fn test_get_mut() {
        let arr = AtomicArray::new();
        arr.push(100).unwrap();

        {
            let mut val = arr.get_mut(0).unwrap();
            *val = 200;
        }

        assert_eq!(*arr.get(0).unwrap(), 200);
    }

    #[test]
    fn test_as_vec() {
        let arr = AtomicArray::from_iter(0..5);
        let vec = arr.as_vec();
        assert_eq!(vec, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_reset_with_initializer() {
        let arr: AtomicArray<usize> = AtomicArray::new();

        let result = arr.reset_with(10, || 7);
        assert_eq!(result, Ok(10));
        assert_eq!(arr.len(), 10);

        for i in 0..10 {
            assert_eq!(*arr.get(i).unwrap(), 7);
        }
    }

    #[test]
    fn test_clone_and_refcount() {
        let arr = AtomicArray::from_iter(vec![1, 2, 3]);
        let clone = arr.clone();

        assert_eq!(arr.len(), 3);
        assert_eq!(clone.len(), 3);
        assert_eq!(*clone.get(1).unwrap(), 2);
    }

    #[test]
    fn test_multithreaded_push() {
        let arr = Arc::new(AtomicArray::with_capacity(100));
        let threads: Vec<_> = (0..10)
            .map(|i| {
                let arr = Arc::clone(&arr);
                thread::spawn(move || {
                    for j in 0..10 {
                        let _ = arr.push(i * 10 + j);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(arr.len(), 100);
    }

    #[test]
    fn test_multithreaded_get() {
        let arr = Arc::new(AtomicArray::from_iter(0..100));
        let threads: Vec<_> = (0..10)
            .map(|_| {
                let arr = Arc::clone(&arr);
                thread::spawn(move || {
                    let mut sum = 0;
                    for i in 0..100 {
                        if let Some(v) = arr.get(i) {
                            sum += *v;
                        }
                    }
                    sum
                })
            })
            .collect();

        let total: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        // 0 + 1 + ... + 99 = 4950, 10 threads => total should be 49500
        assert_eq!(total, 49500);
    }
}
