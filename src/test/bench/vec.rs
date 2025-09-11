use crate::atomic::AtomicVec;
use std::thread;
use std::time::Instant;

#[test]
#[cfg_attr(miri, ignore)]
fn dfvgsfv() {
    const N: usize = 1_000_000;

    // === Single-thread push/pop ===
    let q = AtomicVec::new();
    let start = Instant::now();
    for i in 0..N {
        q.push(i);
    }
    for _i in 0..N {
        q.pop().unwrap();
    }
    let duration = start.elapsed();
    println!("Single-thread push+pop: {:.2?}", duration);

    // === Multi-threaded push ===
    let q = AtomicVec::new();
    let start = Instant::now();
    let mut handles = Vec::new();
    let threads = 12;
    let per_thread = N / threads;

    for i in 0..threads {
        let q = q.clone();
        handles.push(thread::spawn(move || {
            for j in 0..per_thread {
                q.push(i * per_thread + j);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let duration = start.elapsed();
    println!(
        "Multi-threaded push ({} threads): {:.2?}",
        threads, duration
    );

    // === Multi-threaded push + pop ===
    let q = AtomicVec::new();
    let start = Instant::now();
    let mut handles = Vec::new();

    // Pushers
    for i in 0..threads {
        let q = q.clone();
        handles.push(thread::spawn(move || {
            for j in 0..per_thread {
                q.push(i * per_thread + j);
            }
        }));
    }

    // Poppers
    for _ in 0..threads {
        let q = q.clone();
        handles.push(thread::spawn(move || {
            let mut count = 0;
            while count < per_thread {
                if q.pop().is_some() {
                    count += 1;
                }
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let duration = start.elapsed();
    println!(
        "Multi-threaded push + pop ({} threads): {:.2?}",
        threads * 2,
        duration
    );
}
