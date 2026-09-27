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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Build {
    #[schemars(extend("minProperties" = 1))]
    pub targets: BTreeMap<TargetProfileId, TargetProfile>,
    /// Distribution artifacts this project publishes.
    ///
    /// Declaring artifacts is optional. A project that declares none builds one
    /// installer per selected target, which is the simplest thing that works.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<ArtifactId, ArtifactProfile>,
}

/// Stable name of one declared distribution artifact.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[schemars(transparent)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, zup_core::ValueError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(zup_core::ValueError::Empty {
                kind: "artifact id",
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for ArtifactId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// How much of a target set one artifact file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// One file carrying every included target, selected at run time.
    Universal,
    /// One file carrying exactly one target.
    Single,
}

/// Whether an artifact carries its content or fetches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    /// Every required byte is inside the artifact. No network is used.
    Offline,
    /// The artifact carries what it needs to start and select a variant; the rest
    /// is fetched by digest through the update trust configuration.
    Thin,
}

/// One declared distribution artifact.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProfile {
    #[serde(default = "default_artifact_kind")]
    pub kind: ArtifactKind,
    #[serde(default = "default_artifact_mode")]
    pub mode: ArtifactMode,
    /// The target profiles this artifact includes. Required.
    pub targets: Vec<TargetProfileId>,
    /// The release channel this artifact follows.
    ///
    /// An artifact without a channel is labelled with an exact version and always
    /// installs it. An artifact with one installs the channel's current release,
    /// which is a different promise and a different file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The output file name. Derived from the application name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

fn default_artifact_kind() -> ArtifactKind {
    ArtifactKind::Universal
}

fn default_artifact_mode() -> ArtifactMode {
    ArtifactMode::Offline
}

/// Build-time update repository settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    pub repository: String,
    pub channel: String,
    pub root: std::path::PathBuf,
}
