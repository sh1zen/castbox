
# 📦 A Runtime-Typed Reference-Counted Smart Pointer and concurrent programming tools.


---

## ✨ AtomicVec — Lock-Free Atomic Vector for Rust

**AtomicVec** is an internal lock-spin-park, thread-safe vector optimized for concurrent insertions and removals. It uses atomic pointers and internal backoff strategies to allow multiple threads to push and pop items without blocking, while maintaining safe memory management via reference counting.

- ✅ Internal lock-spin-park push and pop operations
- 🔁 Automatic reference counting for safe sharing
- 🔐 Thread-safe without kernel-level mutexes
- ⚡ Optimized for high-concurrency workloads
- 🧠 Suitable for implementing queues, stacks, and other dynamic collections

## ✨ AtomicHashMap — Lock-Free Concurrent Hash Map for Rust

**AtomicHashMap** is a high-performance, internal lock-spin-park hash map designed for multi-threaded access. Buckets are individually protected with lightweight locks, allowing concurrent reads, writes, and removals with minimal contention. Supports both immutable and mutable guarded access to stored values.

- ✅ Internal lock-spin-park concurrent insert, remove, and lookup
- 🔁 Safe iteration and mutable access via WatchGuards
- 🔐 Lightweight per-bucket locking
- ⚡ Optimized for high concurrency and low contention
- 🧠 Ideal for shared caches, state maps, and runtime-managed data

## ✨ Arw — Atomic Reference Counted Mutable

**Arw** is an atomic smart pointer with fine-grained internal locking and strong/weak reference counting. It provides safe data sharing across threads, controlled concurrent access, and raw pointer conversions without relying on kernel-level mutexes.

- ✅ Atomic strong/weak reference counting
- 🔐 Fine-grained internal lock for mutable or shared access
- 🔁 Safe downgrade to WeakArw and try_unwrap for unique value recovery
- ⚡ Optimized for concurrency with low overhead (spin + atomic fences)
- 🧠 Suitable for shared data structures, caches, and custom concurrent primitives

## ✨ Mutex — User-Space Fast Mutex for Rust

**Mutex** is a high-performance user-space mutex supporting exclusive and group locks. Built on atomic primitives and exponential backoff, it minimizes kernel-level contention while providing safe multi-threaded access control.

- ✅ Exclusive and group locking modes
- 🔁 Reference-counted for safe cloning
- 🔐 Lock-free spinning with backoff and thread parking
- ⚡ Extremely low overhead for fast lock/unlock cycles
- 🧠 Suitable for performance-critical synchronization scenarios

## ✨ AnyRef — Runtime-Typed Reference-Counted Smart Pointer for Rust

**AnyRef** is a custom smart pointer similar to `Arc`, designed for storing dynamically typed (`dyn Any`) values with strong and weak reference support, runtime downcasting, and optional thread-safe interior mutability.  
It is ideal for scenarios where type erasure and runtime polymorphism are needed without exposing generic interfaces.

- ✅ Runtime type storage via `dyn Any`
- 🔁 Strong and weak reference counting
- 🔐 Thread-safe mutability (with internal locking)
- 🔍 Safe runtime downcasting (`try_downcast`, `try_downcast_mut`)
- 🧠 Suitable for runtime-managed object graphs

---

## ⚙️ Example Usage

### AtomicHashMap

```rust
use std::thread;
use castbox::collections::AtomicHashMap;
    
let h = AtomicHashMap::new();

h.insert("c", "hello");
let b = h.clone();
drop(h);

{
    let b = b.clone();
    let t = thread::spawn(move || {
        if let Some(mut v) = b.get_mut("c") {
            *v = "world"
        }
    });
    t.join().unwrap();
}

assert_eq!(b.get("c").unwrap(), "world");
```

### AtomicVec

```rust
use std::thread;
use castbox::collections::AtomicVec;
    
let h = AtomicVec::new();

h.push("hello");
let b = h.clone();
drop(h);

{
    let b = b.clone();
    let t = thread::spawn(move || {
        if let Some(v) = b.pop() {
            assert_eq!(v, "hello");
        }
    });
    t.join().unwrap();
}

assert!(b.pop().is_none());
```

### AnyRef

```rust
use castbox::{AnyRef, WeakAnyRef};
use std::thread;

let mut handles = vec![];
let a_ref = AnyRef::new(String::from("hello"));

assert_eq!(a_ref.as_ref::<String>(), "hello");
let b = a_ref.clone();
if let Some(s) = a_ref.try_downcast_ref::<String>() {
    assert_eq!(s, "hello");
}
drop(b);
let w = a_ref.downgrade();
assert!(w.upgrade().is_some());

for _ in 0..10 {
    let a_ref = a_ref.clone();
    handles.push(thread::spawn(move || {
        if let Some(mut s) = a_ref.try_downcast_mut::<String>() {
            s.push_str(":1")
        }
    }));
}

for _ in 0..10 {
    let a_ref = a_ref.clone();
    handles.push(thread::spawn(move || {
        if let Some(s) = a_ref.try_downcast_ref::<String>() {
            let _ = std::hint::black_box(s);
        }
    }));
}

for h in handles {
    h.join().unwrap();
}

assert_eq!(a_ref.try_downcast_ref::<String>().unwrap().len(), 25);

drop(a_ref);
assert!(w.upgrade().is_none());

assert_eq!(WeakAnyRef::strong_count(&w), 0);
assert_eq!(WeakAnyRef::weak_count(&w), 1);
```

### Mutex

```rust
use castbox::mutex::Mutex;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

let mutex = Mutex::new();

let m1 = mutex.clone();
let m2 = mutex.clone();

mutex.lock_group();
mutex.lock_group();

mutex.unlock_group();
mutex.unlock_group();

let h1 = thread::spawn(move || {
    m1.lock_exclusive();
    sleep(Duration::from_millis(100));
    m1.unlock_exclusive();
});

let h2 = thread::spawn(move || {
    m2.lock_exclusive();
    m2.unlock_exclusive();
});

h1.join().unwrap();
h2.join().unwrap();

drop(mutex);
```

---

## 📦 Installation

Install AnyRef from crates.io
Open your Cargo.toml and add:

```toml
[dependencies]
castbox = "0.0.9" # or the latest version available 
```
---

## 📄 License

Apache-2.0

---

## 🔬 Disclaimer

This library is experimental and intended for educational or internal use cases. It manipulates raw pointers, uses `unsafe`, and reimplements low-level synchronization mechanisms. Use with caution in production code.
