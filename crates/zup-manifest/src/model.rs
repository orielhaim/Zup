//! Typed `zup.toml` authoring model.
//!
//! Values are strongly typed with `zup-core` domain types. TOML-specific
//! structure lives here; the compiled output is `zup_core::Installer`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zup_core::Prerequisite;
use zup_core::{
    App, Component, FileAssociation, FileMapping, Frontend, Install, Launcher, PathEntry, Protocol,
    Service, TargetProfile, TargetProfileId, UiBranding,
};

use crate::plugin::Plugin;

/// Currently supported manifest schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// A resource declaration with optional target-profile applicability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Targeted<T> {
    #[serde(flatten)]
    #[schemars(flatten)]
    pub value: T,
    #[serde(default)]
    pub targets: Vec<TargetProfileId>,
}

impl<T> Targeted<T> {
    /// Whether this declaration applies to `profile`.
    pub fn applies_to(&self, profile: &TargetProfileId) -> bool {
        self.targets.is_empty() || self.targets.contains(profile)
    }
}

/// A parsed and validated `zup.toml` manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub app: App,
    pub frontend: Frontend,
    pub ui: Option<UiBranding>,
    pub build: Build,
    pub install: Install,
    pub prerequisites: Vec<Targeted<Prerequisite>>,
    pub updates: Option<Updates>,
    pub components: Vec<Targeted<Component>>,
    pub plugins: Vec<Targeted<Plugin>>,
    pub files: Vec<Targeted<FileMapping>>,
    pub launchers: Vec<Targeted<Launcher>>,
    pub path: Vec<Targeted<PathEntry>>,
    pub services: Vec<Targeted<Service>>,
    pub protocols: Vec<Targeted<Protocol>>,
    pub file_associations: Vec<Targeted<FileAssociation>>,
}

/// Build configuration and target matrix.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Build {
    #[schemars(extend("minProperties" = 1))]
    pub targets: BTreeMap<TargetProfileId, TargetProfile>,
}

/// Build-time update repository settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    pub repository: String,
    pub channel: String,
    pub root: std::path::PathBuf,
}
