#![doc = include_str!("../README.md")]
#![allow(dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]
#![doc(test(
    no_crate_inject,
    attr(
        deny(warnings, rust_2018_idioms),
        allow(dead_code, unused_assignments, unused_variables)
    )
))]
#![warn(
    rust_2024_compatibility,
    rust_2018_idioms,
    rustdoc::broken_intra_doc_links,
    unreachable_pub
)]

mod anyref;
mod arw;

pub mod utils;

pub use anyref::{AnyRef, WeakAnyRef};
pub use arw::{Arw, WeakArw};
#[cfg(test)]
mod test;
