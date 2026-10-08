use semver::Version;
use serde::{Deserialize, Serialize};

use crate::model::{
    App, Component, ComponentGroup, FileAssociation, FileMapping, Frontend, Install, Launcher,
    PathEntry, PluginBinding, Protocol, Service,
};
use crate::prerequisite::Prerequisite;
use crate::target::TargetTriple;
use crate::{NonEmptyString, Sha256Digest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub app: App,
    pub target: TargetTriple,
    #[serde(default)]
    pub frontend: Frontend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<PresetRuntime>,
    pub updates: Option<UpdateConfig>,
    pub install: Install,
    #[serde(default)]
    pub prerequisites: Vec<Prerequisite>,
    pub components: Vec<Component>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub component_groups: Vec<ComponentGroup>,
    pub plugins: Vec<PluginBinding>,
    pub files: Vec<FileMapping>,
    pub launchers: Vec<Launcher>,
    pub path: Vec<PathEntry>,
    pub services: Vec<Service>,
    pub protocols: Vec<Protocol>,
    pub file_associations: Vec<FileAssociation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetRuntime {
    pub name: NonEmptyString,
    pub version: Version,
    pub protocol: u32,
    pub required_capabilities: Vec<String>,
    pub settings: serde_json::Value,
    /// under `sha256`, so a preset never learns a source path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<PresetAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetAsset {
    pub name: NonEmptyString,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledPreset {
    pub preset: PresetRuntime,
    pub executable: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub repository: String,
    pub channel: String,
    pub trusted_root: Vec<u8>,
}
