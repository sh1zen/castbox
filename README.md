
# 📦 Blazingly fast concurrent Data Structures.


- 🪪 Thread-safe with spin-lock backoff and kernel-level mutexes
- ⚡ Optimized for high-concurrency workloads
- 💾 Safe memory management via reference counting to optimize cloning
- 🔐 Internal mutability, just use &self

---

## ✨ AtomicVec

**AtomicVec** is a lock-free, thread-safe vector in Rust designed for high-concurrency environments. It supports atomic push and pop operations with minimal blocking, maintaining safe memory management through ManuallyDrop and reference counting.

- 🧠 Suitable for implementing queues, stacks, and other dynamic collections

## ✨ AtomicHashMap 

**AtomicHashMap** is a thread-safe, concurrent hash map in Rust that supports high-performance insertion, retrieval, and removal of key-value pairs. It uses fine-grained atomic operations combined with internal mutexes to manage contention efficiently.

- 🧠 Ideal for Shared caches, state maps, and runtime-managed data

## ✨ AtomicChain — Multi value Hash Map 

**AtomicChain** is a high-performance, thread-safe key-value store implemented in Rust. It is designed for concurrent access without blocking, using fine-grained atomic operations and internal mutexes to manage contention.

- 🧠 Supports multiple values per key and iteration without copying
- 📏 Resizable bucket array to optimize hash distribution and performance

## ✨ Arw — Atomic Reference Counted Mutable

**Arw** is an atomic smart pointer with fine-grained internal locking and strong/weak reference counting. It provides safe data sharing across threads, controlled concurrent access, and raw pointer conversions without relying on kernel-level mutexes.

- 🧠 Suitable for Shared data structures, caches, and custom concurrent primitives

## ✨ AnyRef — Runtime-Typed Reference-Counted Smart Pointer 

**AnyRef** is a custom smart pointer similar to `Arc`, designed for storing dynamically typed (`dyn Any`) values with strong and weak reference support, runtime downcasting, and optional thread-safe interior mutability.  
It is ideal for scenarios where type erasure and runtime polymorphism are needed without exposing generic interfaces.

- 🧠 Safe runtime downcasting (`try_downcast`, `try_downcast_mut`)

## ✨ Mutex — Fast raw locking 

**Mutex** is a high-performance user-space mutex supporting exclusive and group locks. Built on atomic primitives and exponential backoff, it minimizes kernel-level contention while providing safe multi-threaded access control.

- 🧠 Suitable for performance-critical synchronization scenarios

## ✨ Barrier — Thread Synchronization Primitive 

**Barrier** is a lightweight, thread-safe synchronization primitive in Rust that coordinates groups of threads. It blocks threads until a specified number of waiters arrive, then releases them all simultaneously. Once released, the barrier resets to a configurable capacity (bucket) for reuse.

- 🧠 Suitable for parallel algorithms, phased execution, and workload synchronization

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
use castbox::containers::{AnyRef, WeakAnyRef};

let a_ref = AnyRef::new(String::from("hello"));

assert_eq!(a_ref.as_ref::<String>(), "hello");
let b = a_ref.clone();
if let Some(s) = a_ref.try_downcast_ref::<String>() {
    assert_eq!(s, "hello");
}

let w = a_ref.downgrade();
assert!(w.upgrade().is_some());

if let Some(mut s) = w.upgrade().unwrap().try_downcast_mut::<String>() {
    s.push_str(":1")
}

assert_eq!(WeakAnyRef::strong_count(&w), 2);
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
    sleep(Duration::from_millis(10));
    m1.unlock_exclusive();
});

let h2 = thread::spawn(move || {
    m2.lock_exclusive();
    m2.unlock_exclusive();
});

h1.join().unwrap();
h2.join().unwrap();
```

---

## 📦 Installation

Install castbox from crates.io
Open your Cargo.toml and add:

```toml
[dependencies]
castbox = "0.0.12" # or the latest version available 
```
---

## 📄 License

Apache-2.0

---

## 🔬 Disclaimer

This library is experimental and intended for educational or internal use cases. It manipulates raw pointers, uses `unsafe`, and reimplements low-level synchronization mechanisms. Use with caution in production code.
