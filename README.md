# castbox — Rust smart pointers with runtime downcasting

`castbox` is a Rust library for thread-safe, reference-counted smart pointers with runtime downcasting and interior
mutability. It combines shared ownership, strong and weak references, and read-write locking for access to shared values.

`castbox` provides two pointer types: `AnyRef` stores a type-erased value, while `Arw<T>` retains its type at compile
time. Both support strong and weak references and protect value access with a read-write lock from `crossync`.

This documentation describes the `0.2.0` source tree. Changes and migration details are listed in
the [changelog](https://github.com/sh1zen/castbox/blob/main/CHANGELOG.md).

[Rust API documentation](https://docs.rs/castbox) · [crates.io package](https://crates.io/crates/castbox) · [Source repository](https://github.com/sh1zen/castbox)

## Install the Rust crate

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
castbox = "0.2.0"
```

## Choose between AnyRef and `Arw<T>`

|                        | `AnyRef`                                     | `Arw<T>`                  |
|------------------------|----------------------------------------------|---------------------------|
| Value type             | Erased; checked at runtime                   | Known at compile time     |
| Read access            | `as_ref::<T>()` or `try_downcast_ref::<T>()` | `as_ref()`                |
| Write access           | `as_mut::<T>()` or `try_downcast_mut::<T>()` | `as_mut()`                |
| Replacement            | May change the value's type                  | Preserves `T`             |
| Weak reference         | `WeakAnyRef`                                 | `WeakArw<T>`              |
| Sharing across threads | All payloads require `Any + Send + Sync`     | Requires `T: Send + Sync` |

Use `AnyRef` for heterogeneous collections or interfaces that need to hide the concrete value type. Use `Arw<T>` when
callers know the type and need shared mutable access. `Arw::new` requires `T: 'static`; unlike `AnyRef`, it can store
values without `Send + Sync` for use within one thread.

## Compare with Rust's Arc, Rc, and RwLock

Rust's [`Rc<T>`](https://doc.rust-lang.org/std/rc/struct.Rc.html) provides reference-counted ownership within one thread.
[`Arc<T>`](https://doc.rust-lang.org/std/sync/struct.Arc.html) uses atomic reference counts and can share ownership across
threads when `T` satisfies the required bounds. Shared ownership alone does not provide mutable access;
`Arc<RwLock<T>>` is a standard way to combine ownership with read-write locking.

`Arw<T>` exposes cloning, weak references, and read-write guards through one pointer API. `AnyRef` adds type erasure
with `dyn Any`, checked downcasting, and shared replacement that can change the value's concrete type. These APIs are
useful for heterogeneous object registries and shared application state where callers need synchronized access to
typed or dynamically typed values.

## Shared ownership and interior mutability

Cloning a pointer creates another owner of the same allocation. It does not clone the value. Read and write access
return guards; dereference a guard to access the value and drop it to release the lock.

```rust
use castbox::Arw;

let values = Arw::new(vec![1, 2]);
let shared = values.clone();

{
let mut guard = shared.as_mut();
guard.push(3);
}

assert_eq!(*values.as_ref(), vec![1, 2, 3]);
assert!(Arw::ptr_eq(&values, &shared));
assert_eq!(Arw::strong_count(&values), 2);
```

## Runtime downcasting with AnyRef

`AnyRef` checks the requested type while holding the value lock. The `try_downcast_*` methods return `None` on a type
mismatch; `as_ref::<T>()` and `as_mut::<T>()` panic instead. All of these methods wait for the corresponding lock,
including those with `try` in their names.

```rust
use castbox::AnyRef;

let value = AnyRef::new(String::from("hello"));

if let Some( mut text) = value.try_downcast_mut::<String>() {
text.push_str(" world");
}

assert_eq!(*value.as_ref::<String>(), "hello world");
assert!(value.try_downcast_ref::<i32>().is_none());
```

`AnyRef` requires `Any + Send + Sync` for every inserted value, including boxed values, defaults, mapped results, and
replacements. `Any` requires a `'static` value. Types such as `Rc<T>` and `RefCell<T>` do not satisfy the thread-sharing
bounds.

## Thread-safe access with read-write guards

Read guards permit concurrent readers; write guards provide exclusive access. Synchronization applies to every clone of
the same allocation.

```rust
use castbox::Arw;
use std::thread;

let counter = Arw::new(0usize);
let mut workers = Vec::new();

for _ in 0..4 {
let shared = counter.clone();
workers.push(thread::spawn( move | | {
* shared.as_mut() += 1;
}));
}

for worker in workers {
worker.join().unwrap();
}

assert_eq!(*counter.as_ref(), 4);
```

Keep guards short-lived. Release a read guard before requesting write access, and release a write guard before
requesting any further value access through the same pointer or a clone. Nested access can deadlock. Replacement and
mapping also acquire value locks. Pointer formatting with `{:p}` prints the allocation address without acquiring a value
lock.

## Replacing and mapping values

`fill` replaces the value in the existing allocation and returns the consumed handle. Every clone observes the
replacement. For `AnyRef`, replacement also updates the reported type name and can invalidate subsequent downcasts to
the previous type.

```rust
use castbox::AnyRef;

let value = AnyRef::new(42i32);
let shared = value.clone();
let value = AnyRef::fill(value, String::from("ready"));

assert!(AnyRef::ptr_eq(&value, &shared));
assert!(shared.try_downcast_ref::<i32>().is_none());
assert_eq!(*shared.as_ref::<String>(), "ready");
assert_eq!(shared.type_name(), std::any::type_name::<String>());
```

The previous value is dropped after releasing the write lock. If its destructor panics, surviving clones retain the
replacement.

`map` consumes a handle and creates a separate allocation from the closure's result. Other owners of the original
allocation retain their original value. `AnyRef::map` passes a read guard to the closure; `Arw::map` passes `&T`.

```rust
use castbox::{AnyRef, WatchGuardRef};

let text = AnyRef::new(String::from("hello"));
let length = text.map( | value: WatchGuardRef<'_, String> | value.len());

assert_eq!(*length.as_ref::<usize>(), 5);
```

## Strong and weak reference counting

`downgrade` creates a weak reference without keeping the value alive. `upgrade` returns a new strong reference while the
value is alive, or `None` once all strong references have been dropped. Weak handles retain the allocation needed for
reference counting until they are also dropped.

```rust
use castbox::AnyRef;

let value = AnyRef::new(42i32);
let weak = value.downgrade();

assert!(weak.upgrade().is_some());
assert_eq!(weak.weak_count(), 1);

drop(value);
assert!(weak.upgrade().is_none());
assert_eq!(weak.strong_count(), 0);
```

`weak_count` counts explicit weak handles and excludes the allocation's internal weak reference. Reference counts are
observations that may change immediately when other threads clone or drop handles; they do not grant exclusive access.

For both pointer types, `try_unwrap` requires exactly one strong reference and no explicit weak references.
`AnyRef::try_unwrap::<T>` also requires a matching value type. Failure returns the original handle in `Err`. `is_unique`
checks for one strong reference and no explicit weak references, but does not reserve ownership.

```rust
use castbox::Arw;

let value = Arw::new(String::from("owned"));
let weak = value.downgrade();
let value = Arw::try_unwrap(value).expect_err("a weak handle still exists");

drop(weak);
assert_eq!(Arw::try_unwrap(value).unwrap(), "owned");
```

## Raw pointer ownership and API migration

Prefer constructors and value guards for ordinary use. Version `0.2.0` introduces these changes to the raw
ownership API:

| Previous API                             | Current API                                      |
|------------------------------------------|--------------------------------------------------|
| Safe `From<*mut T>` conversion           | Unsafe `from_owned_raw(ptr)`                     |
| Payload pointer returned by `into_raw()` | Opaque `*const ()` ownership pointer             |
| Safe `utils::dealloc_layout(ptr)`        | Unsafe call with explicit ownership requirements |

`into_raw` consumes a strong handle without decrementing its count. Restore the ownership pointer exactly once with the
matching `from_raw` function. For `Arw<T>`, restore it with the original `T`. The pointer refers to the shared
allocation and must not be dereferenced as a payload pointer. Failing to restore it leaks the retained ownership.

```rust
use castbox::Arw;

let raw = Arw::new(String::from("retained")).into_raw();

// SAFETY: This pointer came from Arw<String>::into_raw and has not been restored.
let value = unsafe { Arw::<String>::from_raw(raw) };
assert_eq!(*value.as_ref(), "retained");
```

`from_owned_raw` accepts exclusive ownership of a value and its allocation from `Box::into_raw` or
`utils::create_raw_pointer`. The pointer must be valid, initialized, and unconsumed for the same `T`. After transfer, do
not access or free it again. These payload pointers are distinct from the opaque pointers produced by a smart pointer's
`into_raw`.

```rust
use castbox::AnyRef;

let raw = Box::into_raw(Box::new(42i32));

// SAFETY: The pointer owns an initialized i32 allocated by Box and is transferred once.
let value = unsafe { AnyRef::from_owned_raw(raw) };
assert_eq!(*value.as_ref::<i32>(), 42);
```

`utils::dealloc_raw_pointer` drops a value and frees its allocation. `utils::dealloc_layout` frees only the allocation;
the caller must have already moved or dropped the value. Both are unsafe and require exclusive ownership of a compatible
allocation. See their API documentation for the complete contracts.

## Development

Run the Rust checks from the repository root:

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
```

Workflow tests require Python 3, Bash, and Git. They execute the workflow scripts against temporary repositories and a
simulated GitHub CLI, without making GitHub API calls:

```sh
python3 -m unittest discover -s tests -p 'test_workflows.py' -v
```

Rust CI runs on Linux, macOS, and Windows; workflow script tests run on Linux. To check memory access and
reference-counting behavior with Miri, install its nightly components and run:

```sh
rustup component add --toolchain nightly miri rust-src
cargo +nightly miri test --lib --test regressions --target x86_64-unknown-linux-gnu
```

## License

Licensed under [Apache-2.0](https://github.com/sh1zen/castbox/blob/main/LICENSE).
