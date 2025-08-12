use castbox::{AnyRef, Downcast};
use std::rc::Rc;
use std::sync::Barrier;
use std::thread;
use std::time::Instant;

struct UnsafeSendRc<T>(Rc<T>);
impl<T> UnsafeSendRc<T> {
    fn new(val: T) -> UnsafeSendRc<T> {
        UnsafeSendRc(Rc::new(val))
    }
}
impl<T> Clone for UnsafeSendRc<T> {
    fn clone(&self) -> Self {
        UnsafeSendRc(self.0.clone())
    }
}

fn main() {
    let rc = UnsafeSendRc::new(Vec::from([1i32, 2i32, 3i32]));
    let mut handles = vec![];
    let barrier = AnyRef::new(Barrier::new(1_000));

    let a_rc = AnyRef::new(rc.clone());

    let start = Instant::now();

    for i in 0..100 {
        let mut x_clone = a_rc.clone();
        let barrier_clone = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier_clone.downcast_ref::<Barrier>().wait();
            for _ in 0..100 {
                let xx_clone = x_clone.downcast_mut::<UnsafeSendRc<Vec<i32>>>();
                // add dirty
                let _clone = std::hint::black_box(&xx_clone);

                // direct retrieve rc data ptr
                let ptr = &raw const (*xx_clone.0) as *mut Vec<i32>;
                let ptr = unsafe { &mut *ptr };
                ptr.push(i);
            }
        }));
    }

    for _ in 0..100 {
        let a_rc_clone = a_rc.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                let _clone = a_rc_clone.as_ref::<UnsafeSendRc<Vec<i32>>>();
                std::hint::black_box(&_clone);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    println!("elapsed: {:?}", start.elapsed());

    //assert_eq!(rc.0.as_ref().len(), 10_000_003);

    println!("Fatto");
}
