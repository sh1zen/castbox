#[cfg(test)]
mod tests_atomic_buffer {
    use std::sync::Arc;
    use std::thread;
    use crate::atomic::AtomicBuffer;

    #[test]
    fn test_single_push_pop() {
        let buffer = AtomicBuffer::<i32>::new();
        let val = Box::into_raw(Box::new(42));

        assert!(buffer.push(val).is_ok());

        let popped = buffer.pop().unwrap();
        unsafe {
            assert_eq!(*popped, 42);
            drop(Box::from_raw(popped)); // libera memoria
        }
    }

    #[test]
    fn test_fill_buffer() {
        let buffer = AtomicBuffer::<i32>::new();

        for i in 0..buffer.capacity() {
            let val = Box::into_raw(Box::new(i as i32));
            assert!(buffer.push(val).is_ok());
        }

        // Next push should fail (buffer full)
        let val = Box::into_raw(Box::new(999));
        assert!(buffer.push(val).is_err());

        // Rilascio immediatamente la memoria dell'elemento non inserito
        unsafe { drop(Box::from_raw(val)) };

        // svuoto tutto per non lasciare leak
        for _ in 0..buffer.capacity() {
            let ptr = buffer.pop().unwrap();
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }

    #[test]
    fn test_pop_all() {
        let buffer = AtomicBuffer::<i32>::new();

        for i in 0..buffer.capacity() {
            let val = Box::into_raw(Box::new(i as i32));
            assert!(buffer.push(val).is_ok());
        }

        for i in 0..buffer.capacity() {
            let val = buffer.pop().unwrap();
            unsafe {
                assert_eq!(*val, i as i32);
                drop(Box::from_raw(val)); // deallocazione
            }
        }

        // Ora dovrebbe essere vuoto
        assert!(buffer.pop().is_none());
    }

    #[test]
    fn test_concurrent_push_pop() {
        let buffer = Arc::new(AtomicBuffer::<i32>::new());

        let producer = {
            let buffer = Arc::clone(&buffer);
            thread::spawn(move || {
                for i in 0..buffer.capacity() {
                    let val = Box::into_raw(Box::new(i as i32));
                    while buffer.push(val).is_err() {
                        // spin finché non c'è spazio
                    }
                }
            })
        };

        let consumer = {
            let buffer = Arc::clone(&buffer);
            thread::spawn(move || {
                let mut count = 0;
                while count < buffer.capacity() {
                    if let Some(ptr) = buffer.pop() {
                        unsafe {
                            // println!("Popped: {}", *ptr);
                            drop(Box::from_raw(ptr));
                        }
                        count += 1;
                    }
                }
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();

        // Alla fine deve essere vuoto
        assert!(buffer.pop().is_none());
    }
}
