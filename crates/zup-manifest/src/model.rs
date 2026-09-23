//! Typed `zup.toml` authoring model.
//!
//! Values are strongly typed with `zup-core` domain types. TOML-specific
//! structure lives here; the compiled output is `zup_core::Installer`.

use zup_core::{
    Action, App, Component, FileMapping, FileType, Install, PathEntry, Protocol, Service, Shortcut,
    Source,
};

/// Currently supported manifest schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// A parsed and validated `zup.toml` manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub app: App,
    pub source: Source,
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
