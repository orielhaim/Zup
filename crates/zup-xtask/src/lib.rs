//! Repository automation for zup.
//!
//! Four jobs: keep one authoritative package matrix, keep the portable stack
//! free of Windows-only code, name the build inputs the artifact tests require,
//! and keep the GitHub Actions zup's workflows depend on pinned and current.
//!
//! The first three read the workspace from disk and never shell out to cargo, so
//! they behave identically on every host and need no network. The fourth is the
//! exception and is split in two for that reason: `pins::check` is offline and
//! runs in CI, `pins::refresh` reaches GitHub and is a thing a person runs.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod dispatcher;
pub mod matrix;
pub mod pins;
pub mod workspace;
