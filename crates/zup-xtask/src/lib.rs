//! Repository automation for zup.
//!
//! Five jobs: keep one authoritative package matrix, keep the portable stack
//! free of Windows-only code, name the build inputs the artifact tests require,
//! keep the GitHub Actions zup's workflows depend on pinned and current, and
//! stage the local toolchain a contributor's builds compose from.
//!
//! The first three read the workspace from disk and never shell out to cargo, so
//! they behave identically on every host and need no network. `pins` is the
//! exception and is split in two for a related reason: `pins::check` is offline
//! and runs in CI, `pins::refresh` reaches GitHub and is a thing a person runs.
//! `toolchain` shells out to cargo because its whole job is to produce binaries
//! cargo has to build, and it is only ever run by a person.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod dispatcher;
pub mod matrix;
pub mod pins;
pub mod toolchain;
pub mod workspace;
