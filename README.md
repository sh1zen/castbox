
# 📦 AnyRef — Runtime-Typed Reference-Counted Smart Pointer for Rust

**AnyRef** is a custom smart pointer similar to `Arc`, designed for storing dynamically typed (`dyn Any`) values with strong and weak reference support, runtime downcasting, and optional thread-safe interior mutability.  
It is ideal for scenarios where type erasure and runtime polymorphism are needed without exposing generic interfaces.

---

## ✨ Features

- ✅ Runtime type storage via `dyn Any`
- 🔁 Strong and weak reference counting
- 🔐 Optional thread-safe mutability (with internal locking)
- 🔍 Safe runtime downcasting (`try_downcast`, `try_downcast_mut`)
- 🚫 No generics in the pointer interface
- 🧠 Suitable for runtime-managed object graphs

---

## ⚙️ Example Usage

### Basic Allocation and Access

```rust
use castbox::AnyRef;

let a = AnyRef::new(42i32);
assert_eq!(a.as_ref::<i32>(), &42);
```

### Runtime Downcasting

```rust
use castbox::{AnyRef, Downcast};

let a = AnyRef::new("hello".to_string());
if let Some(s) = a.try_downcast_ref::<String>() {
    assert_eq!(s, "hello");
}
```

### Cloning and Reference Counting

```rust
use castbox::AnyRef;

let a = AnyRef::new(vec![1, 2, 3]);
let b = a.clone();

assert_eq!(AnyRef::strong_count(&a), 2);
```

### Weak Reference

```rust
use castbox::AnyRef;

let a = AnyRef::new("temporary".to_string());
let w = a.downgrade();

assert!(w.upgrade().is_some());
drop(a);
assert!(w.upgrade().is_none());
```

### Thread-Safe Mode

```rust
use std::sync::Barrier;
use std::thread;
use castbox::{AnyRef, Downcast};

let x = AnyRef::new(123i32);
let mut handles = vec![];
let barrier = AnyRef::new(Barrier::new(10));

for i in 0..10 {
    let x_clone = x.clone();
    let barrier_clone = barrier.clone();
    handles.push(thread::spawn(move || {
        barrier_clone.downcast_ref::<Barrier>().wait();
        let val = x_clone.downcast_ref::<i32>();
        assert_eq!(*val, 123);
    }));
    assert_eq!(AnyRef::strong_count(&x), i + 2);
}

for h in handles {
    h.join().unwrap();
}

assert_eq!(AnyRef::strong_count(&x), 1);
assert_eq!(AnyRef::weak_count(&x), 0);
```

---

## 📦 Installation

Install AnyRef from crates.io
Open your Cargo.toml and add:

```toml
[dependencies]
castbox = "0.0.4" #or the latest version available 
```
---

## 📄 License

Apache-2.0

---

## 🔬 Disclaimer

This library is experimental and intended for educational or internal use cases. It manipulates raw pointers, uses `unsafe`, and reimplements low-level synchronization mechanisms. Use with caution in production code.
