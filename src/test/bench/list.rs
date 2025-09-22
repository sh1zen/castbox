use std::thread;
use std::time::Instant;
use crate::atomic::AtomicList;

#[test]
#[cfg_attr(miri, ignore)]
fn bench_list(){
    const NUM_THREADS: usize = 80;
    const OPERATIONS_PER_THREAD: usize = 100_000;

    let list = AtomicList::new(); // unica istanza
    let start_time = Instant::now();

    let mut handles = Vec::new();

    for thread_id in 0..NUM_THREADS {
        let list_clone = list.clone(); // clone "interno", thread-safe

        let handle = thread::spawn(move || {

            for i in 0..OPERATIONS_PER_THREAD {
                let op =i % 4;

                match op {
                    0 => {
                        // 60% push
                        list_clone.push((thread_id, i));
                    }
                    1 => {
                        // 30% pop
                        let _ = list_clone.pop();
                    }
                    2 => {
                        // 5% to_vec (drain)
                        let _ = list_clone.to_vec();
                    }
                    3 => {
                        // 5% read-only iter
                        for val in list_clone.iter() {
                            let _ = val;
                        }
                    }
                    _ => unreachable!(),
                }
            }
        });

        handles.push(handle);
    }

    // Attendi tutti i thread
    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let duration = start_time.elapsed();
    let remaining = list.len();

    println!(
        "Stress test completato in {:?}. Elementi rimanenti: {}",
        duration, remaining
    );

    // Prova a svuotare tutto
    let final_data = list.drain().map(|d| d.collect::<Vec<_>>()).unwrap_or_default();
    println!("Elementi finali drenati: {}", final_data.len());
}