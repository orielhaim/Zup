use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ComponentId, Condition, Installer, NonEmptyString, PluginId, PrerequisiteId, RelativePath,
    Sha256Digest, TargetTriple, Template,
};

/// `source` is a **build-machine** path and must never become portable bundle
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFile {
    pub source: PathBuf,
    pub source_relative: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub component: Option<ComponentId>,
    pub condition: Option<Condition>,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPrerequisite {
    pub id: PrerequisiteId,
    pub source: PathBuf,
    pub source_relative: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAsset {
    pub name: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_relative: Option<RelativePath>,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPlugin {
    pub id: PluginId,
    pub source: PathBuf,
    pub source_relative: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconRole {
    Windows,
    MacOs,
    LinuxSvg,
    LinuxPng { size: u32 },
    Png { size: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledIcon {
    pub role: IconRole,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TargetIcons {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<CompiledIcon>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
}

impl TargetIcons {
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty() && self.warnings.is_empty() && !self.fallback
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetBuildPlan {
    pub installer: Installer,
    pub prerequisites: Vec<ResolvedPrerequisite>,
    pub plugins: Vec<ResolvedPlugin>,
    pub files: Vec<ResolvedFile>,
    #[serde(default)]
    pub ui_assets: Vec<ResolvedAsset>,
    pub total_size: u64,
    pub prerequisite_size: u64,
    #[serde(default, skip_serializing_if = "TargetIcons::is_empty")]
    pub icons: TargetIcons,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    pub targets: Vec<TargetBuildPlan>,
}

impl TargetBuildPlan {
    pub fn sort_key(file: &ResolvedFile) -> (String, String) {
        (
            file.destination.to_string(),
            file.source_relative.as_str().to_owned(),
        )
    }
}

impl BuildPlan {
    pub fn target_by_triple(&self, target: &TargetTriple) -> Option<&TargetBuildPlan> {
        self.targets
            .iter()
            .find(|plan| &plan.installer.target == target)
    }
}
