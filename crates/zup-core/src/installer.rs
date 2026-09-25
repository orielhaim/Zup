//! Normalized installer IR.

use serde::{Deserialize, Serialize};

use crate::model::{
    App, Component, FileMapping, FileType, Frontend, Install, PathEntry, PluginBinding, Protocol,
    Service, Shortcut, UiBranding,
};
use crate::prerequisite::Prerequisite;

/// Engine-facing installer representation.
///
/// Fully normalized and deterministic. Declaration order is preserved in
/// every collection. This IR is platform-independent and contains only
/// intent: no filesystem discovery, no OS registration, no execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub app: App,
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
    pub shortcuts: Vec<Shortcut>,
    pub path: Vec<PathEntry>,
    pub services: Vec<Service>,
    pub protocols: Vec<Protocol>,
    pub file_types: Vec<FileType>,
}

/// Runtime update trust configuration embedded by the build step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub repository: String,
    pub channel: String,
    pub trusted_root: Vec<u8>,
}
