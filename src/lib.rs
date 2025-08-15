#![doc = include_str!("../README.md")]

#![doc(test(
    no_crate_inject,
    attr(
        deny(warnings, rust_2018_idioms),
        allow(dead_code, unused_assignments, unused_variables)
    )
))]

#![warn(
    missing_debug_implementations,
    rust_2024_compatibility,
    rust_2018_idioms,
    rustdoc::broken_intra_doc_links,
    unreachable_pub
)]
//  missing_docs,

mod any_ref;
mod mutex;
mod utils;

mod tests;

pub use any_ref::{AnyRef, Downcast, WeakAnyRef};
pub use mutex::{Mutex, WatchGuard, AtomicVec};
pub use utils::{create_raw_pointer, dealloc_layout, dealloc_raw_pointer};