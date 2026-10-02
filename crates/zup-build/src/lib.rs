//! Filesystem materialization for zup.
//!
//! Pipeline stage:
//!
//! ```text
//! zup.toml → Manifest → Installer IR → zup-build → zup_core::BuildPlan
//! ```
//!
//! This crate expands declarative `[[files]]` mappings into a concrete,
//! deterministic inventory of real source files (size + SHA-256 + logical
//! destination). It does not install, compress, bundle, or resolve install
//! variables.
//!
//! The inventory it produces is a domain type, owned by `zup-core`, because the
//! runtime reads the same structure out of an installer package without ever
//! seeing the source tree it came from. This crate is the part that walks the
//! tree.

#![forbid(unsafe_code)]

mod digest;
mod error;
mod icons;
mod materialize;
mod pattern;
mod plugins;

pub use zup_core::{
    BuildPlan, ResolvedFile, ResolvedPlugin, ResolvedPrerequisite, TargetBuildPlan,
};

pub use digest::{DigestParseError, Sha256Digest};
pub use error::BuildError;
pub use materialize::{
    MAX_UPDATE_ROOT_BYTES, ResolvedUpdateRoot, Writes, materialize, materialize_destination,
    materialize_with_assets, materialize_with_policy, project_root, resolve_project_source,
    resolve_source_root, resolve_update_root,
};
pub use pattern::FilePattern;
pub use plugins::MAX_PLUGIN_SOURCE_BYTES;
pub use zup_platform::{PortableSourceFilePolicy, SourceFilePolicy};
