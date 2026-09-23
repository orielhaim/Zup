//! Deterministic resolved-file inventory and build plan.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, Condition, Installer, RelativePath, Template};

use crate::digest::Sha256Digest;

/// One materialized source file.
///
/// `source` is a **build-machine** path and must never become portable bundle
/// metadata. Portable identity is `source_relative` plus content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFile {
    /// Absolute (or project-anchored) build-machine location. Not portable.
    pub source: PathBuf,
    /// Portable path relative to the source root, using `/` separators.
    pub source_relative: RelativePath,
    /// Unresolved installer destination template (`${install}` etc. preserved).
    pub destination: Template,
    /// File size in bytes at hashing time.
    pub size: u64,
    /// SHA-256 of the file contents.
    pub sha256: Sha256Digest,
    /// Owning component, if any. Not selected here.
    pub component: Option<ComponentId>,
    /// Install condition, if any. Not evaluated here.
    pub condition: Option<Condition>,
}

/// Deterministic materialization result for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    /// Normalized installer IR the plan was produced from.
    pub installer: Installer,
    /// Resolved payload files, sorted by destination then source_relative.
    pub files: Vec<ResolvedFile>,
    /// Sum of file sizes.
    pub total_size: u64,
}

impl BuildPlan {
    /// Stable sort key for a resolved file.
    pub fn sort_key(file: &ResolvedFile) -> (String, String) {
        (
            file.destination.to_string(),
            file.source_relative.as_str().to_owned(),
        )
    }
}
