//! Repository automation for zup.
//!
//! Three jobs: keep one authoritative package matrix, keep the portable stack
//! free of Windows-only code, and name the build inputs the artifact tests
//! require. The first two read the workspace from disk and never shell out to
//! cargo, so they behave identically on every host.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod dispatcher;
pub mod matrix;
pub mod workspace;
