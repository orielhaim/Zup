//! Normalized installer IR.

use serde::{Deserialize, Serialize};

use crate::model::{
    App, Component, FileAssociation, FileMapping, Frontend, Install, Launcher, PathEntry,
    PluginBinding, Protocol, Service, UiBranding,
};
use crate::prerequisite::Prerequisite;
use crate::target::TargetTriple;

/// Engine-facing installer representation.
///
/// Fully normalized and deterministic. Declaration order is preserved in
/// every collection. This IR is platform-independent and contains only
/// intent: no filesystem discovery, no OS registration, no execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub app: App,
    pub target: TargetTriple,
    #[serde(default)]
    pub frontend: Frontend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiBranding>,
    pub updates: Option<UpdateConfig>,
    pub install: Install,
    #[serde(default)]
    pub prerequisites: Vec<Prerequisite>,
    pub components: Vec<Component>,
    pub plugins: Vec<PluginBinding>,
    pub files: Vec<FileMapping>,
    pub launchers: Vec<Launcher>,
    pub path: Vec<PathEntry>,
    pub services: Vec<Service>,
    pub protocols: Vec<Protocol>,
    pub file_associations: Vec<FileAssociation>,
}

/// Runtime update trust configuration embedded by the build step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub repository: String,
    pub channel: String,
    pub trusted_root: Vec<u8>,
}
