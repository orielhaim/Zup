//! Normalized installer IR.

use serde::{Deserialize, Serialize};

use crate::model::{
    Action, App, Component, FileMapping, FileType, Install, PathEntry, Protocol, Service, Shortcut,
};

/// Engine-facing installer representation.
///
/// Fully normalized and deterministic. Declaration order is preserved in
/// every collection. This IR is platform-independent and contains only
/// intent: no filesystem discovery, no OS registration, no execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub app: App,
    pub install: Install,
    pub components: Vec<Component>,
    pub files: Vec<FileMapping>,
    pub shortcuts: Vec<Shortcut>,
    pub path: Vec<PathEntry>,
    pub services: Vec<Service>,
    pub protocols: Vec<Protocol>,
    pub file_types: Vec<FileType>,
    pub actions: Vec<Action>,
}
