//! Repository automation for zup.
//!
//! Two jobs: keep one authoritative package matrix, and keep the portable
//! stack free of Windows-only code. Both read the workspace from disk and
//! never shell out to cargo, so they behave identically on every host.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod matrix;
pub mod workspace;
