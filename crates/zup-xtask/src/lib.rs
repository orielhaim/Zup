//! Repository automation for zup.
//!
//! Seven jobs: keep one authoritative package matrix, keep the portable stack
//! free of Windows-only code, name the build inputs the artifact tests require,
//! keep the GitHub Actions zup's workflows depend on pinned and current, stage and
//! package the toolchain a release ships, prove that release works from outside
//! this repository, and refuse a dependency graph that grew by accident.
//!
//! The first three read the workspace from disk and never shell out to cargo, so
//! they behave identically on every host and need no network. `pins` is the
//! exception and is split in two for a related reason: `pins::check` is offline
//! and runs in CI, `pins::refresh` reaches GitHub and is a thing a person runs.
//! `toolchain` and `cleanroom` shell out to cargo and to `zup` itself, because
//! their whole job is to run a build and then judge what came out.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod cleanroom;
pub mod dispatcher;
pub mod graph;
pub mod matrix;
pub mod pins;
pub mod toolchain;
pub mod workspace;
