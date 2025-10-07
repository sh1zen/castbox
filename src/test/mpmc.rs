mod tests_mpmc {
    use crate::channels::mpmc::Mpmc;
    use crate::atomic::{AtomicVec};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn mpmc_basic() {
        let ch = Mpmc::new();
        // Lanciamo 3 producer
        let producers: Vec<_> = (0..3)
            .map(|id| {
                let ch = Mpmc::clone(&ch);
                thread::spawn(move || {
                    for i in 0..10 {
                        // invia numero con encoding (id, i)
                        ch.send((id, i)).unwrap();
                        thread::sleep(Duration::from_millis(1));
                    }
                })
            })
            .collect();

        // Lanciamo 4 consumer
        let results = AtomicVec::new(); // riutilizziamo AtomicVec per raccogliere risultati
        let consumers: Vec<_> = (0..4)
            .map(|_| {
                let ch = Mpmc::clone(&ch);
                let results = AtomicVec::clone(&results);
                thread::spawn(move || {
                    while let Some(v) = ch.recv() {
                        results.push(v);
                    }
                })
            })
            .collect();

        // aspetta i producer
        for p in producers {
            p.join().unwrap();
        }

        // chiude il canale: i consumer termineranno dopo aver svuotato il buffer
        ch.close();

        for c in consumers {
            c.join().unwrap();
        }

        // dovremmo avere 3*10 = 30 messaggi
        let mut cnt = 0;
        while let Some(_) = results.pop() {
            cnt += 1;
        }
        assert_eq!(cnt, 30);
    }

    #[test]
    fn test_send_recv() {
        let chan = Mpmc::new();
        assert!(chan.try_recv().is_none());

        chan.send(42).unwrap();
        assert_eq!(chan.try_recv(), Some(42));
        assert!(chan.try_recv().is_none());
    }

    #[test]
    fn test_bounded_channel() {
        let chan = Mpmc::bounded(2);
        assert!(chan.send(1).is_ok());
        assert!(chan.send(2).is_ok());
        // Bounded channel full
        assert!(chan.send(3).is_err());

        assert_eq!(chan.try_recv(), Some(1));
        assert!(chan.send(3).is_ok());
        assert_eq!(chan.try_recv(), Some(2));
        assert_eq!(chan.try_recv(), Some(3));
        assert!(chan.try_recv().is_none());
    }

    #[test]
    fn test_close_channel() {
        let chan = Mpmc::new();
        chan.send(10).unwrap();
        chan.close();

        // Send after close fails
        assert!(chan.send(20).is_err());

        // Receive remaining items
        assert_eq!(chan.try_recv(), Some(10));
        // Empty channel after close returns None
        assert!(chan.try_recv().is_none());
    }

    #[test]
    fn test_clone_refcount() {
        let chan = Mpmc::new();
        let chan2 = chan.clone();
        let chan3 = chan2.clone();

        chan.send(100).unwrap();
        assert_eq!(chan2.try_recv(), Some(100));
        assert!(chan3.try_recv().is_none());
    }

    #[test]
    fn test_multiple_producers_consumers() {
        let chan = Mpmc::new();
        let barrier = Arc::new(Barrier::new(4));

        let mut handles = Vec::new();
        for i in 0..3 {
            let c = chan.clone();
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                b.wait(); // sync start
                for j in 0..5 {
                    c.send(i * 10 + j).unwrap();
                }
            }));
        }

        let c = chan.clone();
        let b = barrier.clone();
        let consumer_handle = thread::spawn(move || {
            b.wait();
            let mut results = Vec::new();
            for _ in 0..15 {
                loop {
                    if let Some(val) = c.recv() {
                        results.push(val);
                        break;
                    }
                }
            }
            results
        });

        for h in handles {
            h.join().unwrap();
        }

        let results = consumer_handle.join().unwrap();
        assert_eq!(results.len(), 15);
        // Optional: check that all expected values are present
        let mut expected: Vec<_> = (0..3)
            .flat_map(|i| (0..5).map(move |j| i * 10 + j))
            .collect();
        let mut actual = results.clone();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_recv_blocks_until_send() {
        let chan = Mpmc::new();

        let c = chan.clone();
        let handle = thread::spawn(move || {
            // recv will block until a value is sent
            let val = c.recv();
            val
        });

        thread::sleep(Duration::from_millis(50)); // let the receiver block
        chan.send(99).unwrap();

        let result = handle.join().unwrap();
        assert_eq!(result, Some(99));
    }

    #[test]
    fn test_recv_after_close_empty() {
        let chan: Mpmc<i32> = Mpmc::new();
        chan.close();
        assert_eq!(chan.recv(), None);
    }
}
