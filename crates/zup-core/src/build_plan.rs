//! The resolved inventory of one target, and of a whole build.
//!
//! These types are the contract between the two halves of the system. The build
//! plane produces them by walking a source tree; the runtime consumes them out of
//! an installer package without ever seeing the tree they came from. That is why
//! they live beside the domain model rather than in `zup-build`: a struct that
//! both halves must name cannot live in either one's crate without the other
//! depending on it.
//!
//! Everything here is data. Nothing reads a file, resolves a pattern, or compiles
//! a manifest.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ComponentId, Condition, Installer, PluginId, PrerequisiteId, RelativePath, Sha256Digest,
    TargetTriple, Template,
};

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

/// A prerequisite package that was found and hashed at build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPrerequisite {
    pub id: PrerequisiteId,
    pub source: PathBuf,
    pub source_relative: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// A plugin module that was found and hashed at build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPlugin {
    pub id: PluginId,
    pub source: PathBuf,
    pub source_relative: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// Materialized sources for one target profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetBuildPlan {
    /// Target-bound normalized installer IR.
    pub installer: Installer,
    /// Resolved embedded prerequisites for this target.
    pub prerequisites: Vec<ResolvedPrerequisite>,
    /// Resolved plugin sources for this target.
    pub plugins: Vec<ResolvedPlugin>,
    /// Resolved payload files, sorted by destination then source relative path.
    pub files: Vec<ResolvedFile>,
    /// Sum of payload file sizes.
    pub total_size: u64,
    /// Sum of embedded prerequisite sizes.
    pub prerequisite_size: u64,
}

/// Deterministic materialization result for all selected target profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    /// Target plans ordered by profile id.
    pub targets: Vec<TargetBuildPlan>,
}

impl TargetBuildPlan {
    /// Stable sort key for a resolved file.
    pub fn sort_key(file: &ResolvedFile) -> (String, String) {
        (
            file.destination.to_string(),
            file.source_relative.as_str().to_owned(),
        )
    }
}

impl BuildPlan {
    /// Find one target plan by its canonical target triple.
    pub fn target_by_triple(&self, target: &TargetTriple) -> Option<&TargetBuildPlan> {
        self.targets
            .iter()
            .find(|plan| &plan.installer.target == target)
    }
}
