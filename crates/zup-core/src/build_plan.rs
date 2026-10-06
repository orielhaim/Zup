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
    ComponentId, Condition, Installer, NonEmptyString, PluginId, PrerequisiteId, RelativePath,
    Sha256Digest, TargetTriple, Template,
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
    /// This file is intended to be executable. Portable intent, not a mode.
    #[serde(default)]
    pub executable: bool,
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

/// One application-provided preset asset, found and hashed at build time.
///
/// The same discipline as a [`ResolvedFile`]: the source is a build-machine fact
/// and is absent wherever the plan was read back out of a package, because a
/// runtime has no source tree to name. Identity is the digest either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAsset {
    /// The name the application's settings used to refer to it.
    pub name: NonEmptyString,
    /// Build-machine location, or `None` when this plan was read from a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// Portable path relative to the project directory, or `None` beside `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_relative: Option<RelativePath>,
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

/// Which generated icon file this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconRole {
    Windows,
    MacOs,
    LinuxSvg,
    LinuxPng { size: u32 },
    Png { size: u32 },
}

/// One icon file produced for a target.
///
/// `source` is a build-machine path into the icon cache. The portable package
/// does not carry it. `executable_images` and `executable_group` are the bytes
/// an executable's icon resources are written from, and only the Windows file
/// has them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledIcon {
    pub role: IconRole,
    /// `/`-separated name, such as `app.ico` or `hicolor/48x48/apps/id.png`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    pub size: u64,
    pub sha256: Sha256Digest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executable_images: Vec<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executable_group: Vec<u8>,
}

/// Icons compiled for one target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TargetIcons {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<CompiledIcon>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The project named no icon, so these bytes came from the built-in mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
}

impl TargetIcons {
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty() && self.warnings.is_empty() && !self.fallback
    }
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
    /// Resolved application preset assets, sorted by name.
    #[serde(default)]
    pub ui_assets: Vec<ResolvedAsset>,
    /// Sum of payload file sizes.
    pub total_size: u64,
    /// Sum of embedded prerequisite sizes.
    pub prerequisite_size: u64,
    /// Icons this target asked for. Empty for a target with no icon surface.
    #[serde(default, skip_serializing_if = "TargetIcons::is_empty")]
    pub icons: TargetIcons,
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
