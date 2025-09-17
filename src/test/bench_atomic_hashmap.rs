use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;
use crate::atomic::AtomicHashMap;

fn benchmark_concurrent_rw(
    n_threads: usize,
    n_operations: usize,
    map: AtomicHashMap<usize, usize>,
) {
    let mut handles = Vec::new();
    let barrier = Arc::new(Barrier::new(n_threads));

    // Populate the map with some initial data
    for i in 0..1000 {
        map.insert(i, i);
    }

    let start = Instant::now();

    for _i in 0..n_threads {
        let map_clone = map.clone();
        let barrier_clone = Arc::clone(&barrier);
        let h = thread::spawn(move || {
            // Wait for all threads to be ready before starting the test
            barrier_clone.wait();

            for op_id in 0..n_operations {
                if op_id % 2 == 0 {
                    // Even operations: write
                    let key = op_id % 1000;
                    map_clone.insert(key, op_id);
                } else {
                    // Odd operations: read
                    let key = op_id % 1000;
                    let _ = map_clone.get(&key);
                }
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    let elapsed = start.elapsed();
    println!(
        "{} threads, {} read/write operations each → {:?}",
        n_threads, n_operations, elapsed
    );
}

#[test]
fn bench_atomic_hashmap_concurrent() {
    let map = AtomicHashMap::new();

    for &threads in &[1, 2, 4, 8, 16] {
        benchmark_concurrent_rw(threads, 100_000, map.clone());
    }
}