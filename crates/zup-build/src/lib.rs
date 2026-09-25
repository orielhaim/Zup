//! Filesystem materialization and deterministic build planning for zup.
//!
//! Pipeline stage:
//!
//! ```text
//! zup.toml → Manifest → Installer IR → zup-build → BuildPlan
//! ```
//!
//! This crate expands declarative `[[files]]` mappings into a concrete,
//! deterministic inventory of real source files (size + SHA-256 + logical
//! destination). It does not install, compress, bundle, or resolve install
//! variables.

#![forbid(unsafe_code)]

mod digest;
mod error;
mod materialize;
mod pattern;
mod plan;
mod plugins;
mod windows;

pub use digest::{DigestParseError, Sha256Digest};
pub use error::BuildError;
pub use materialize::{materialize, materialize_destination};
pub use pattern::FilePattern;
pub use plan::{BuildPlan, ResolvedFile, ResolvedPlugin, ResolvedPrerequisite};
pub use plugins::MAX_PLUGIN_SOURCE_BYTES;
pub use windows::validate_windows_destination;
