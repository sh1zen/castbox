# Changelog

Notable changes to `castbox` are documented below. Breaking API changes are marked explicitly.

## [0.2.0] - 2026-10-09

### Fixed

- Require `Send + Sync` for every payload inserted into `AnyRef`.
- Synchronize replacements and runtime type checks; update type names and debug output.
- Drop replaced values after unlocking, preserving valid state if a destructor panics.
- Count explicit weak references consistently, including during and after payload destruction.
- Handle zero-sized raw allocations and ownership transfer without leaking the source allocation.
- Prevent failed unwraps from making weak upgrades temporarily report a live value as dropped.
- Format pointer addresses without acquiring payload locks, avoiding deadlocks with value guards.
- Support branch names containing slashes in release archive names and propagate archive failures.
- Paginate deployment cleanup, deactivate deployments before deletion, and propagate API failures.
- Replace the obsolete documentation toolchain action and fail documentation builds on warnings.
- Classify stable version tags as regular releases while preserving prerelease status for RC tags and branch builds.
- Enable formatting and Clippy checks in CI with an explicit stable Rust toolchain and locked dependency resolution.

### Changed

- **Breaking:** Require `Any + Send + Sync` for values inserted into `AnyRef`.
- **Breaking:** Replace safe `From<*mut T>` conversions with unsafe `from_owned_raw` constructors that consume ownership of the value and its allocation.
- **Breaking:** Return opaque `*const ()` ownership pointers from `into_raw`, restored exactly once by the matching `from_raw`. For `Arw<T>`, the original `T` must be preserved.
- **Breaking:** Require `unsafe` for `utils::dealloc_layout`; callers must move or drop the value before freeing the allocation.

### Documentation

- Document both pointer types, their locking behavior, weak-reference semantics, and raw ownership contracts.
- Add examples and migration guidance for the updated APIs.
- Document the Rust checks and local workflow test commands.
- Improve the README title, feature descriptions, and package metadata for discovery by Rust developers.

## [0.1.4] - 2026-01-19

### Added

- Expand test coverage for `AnyRef`.
- Expand test coverage for `Arw`.

### Fixed

- Address edge cases in `AnyRef`.
- Address edge cases in `Arw`.

## [0.1.2] - 2025-11-04

### Changed

- Update `crossync` to incorporate dependency bug fixes.

## [0.1.0] - 2025-10-23

### Added

- Introduce `AnyRef` and `Arw` as a separate crate. Synchronization primitives are maintained in [crossync](https://crates.io/crates/crossync).
