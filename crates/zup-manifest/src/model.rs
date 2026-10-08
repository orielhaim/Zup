use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zup_core::Prerequisite;
use zup_core::{
    App, Component, ComponentGroup, FileAssociation, FileMapping, Frontend, Install, Launcher,
    PathEntry, Protocol, Service, TargetProfile, TargetProfileId, Ui,
};

use crate::plugin::Plugin;

pub const SCHEMA_VERSION: u32 = 1;

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
    pub fn applies_to(&self, profile: &TargetProfileId) -> bool {
        self.targets.is_empty() || self.targets.contains(profile)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub app: App,
    pub icon: Option<crate::icon::IconConfig>,
    pub frontend: Frontend,
    pub ui: Ui,
    pub build: Build,
    pub install: Install,
    pub prerequisites: Vec<Targeted<Prerequisite>>,
    pub updates: Option<Updates>,
    pub distribution: Option<Distribution>,
    pub publish: Option<Publish>,
    pub components: Vec<Targeted<Component>>,
    pub component_groups: Vec<Targeted<ComponentGroup>>,
    pub plugins: Vec<Targeted<Plugin>>,
    pub files: Vec<Targeted<FileMapping>>,
    pub launchers: Vec<Targeted<Launcher>>,
    pub path: Vec<Targeted<PathEntry>>,
    pub services: Vec<Targeted<Service>>,
    pub protocols: Vec<Targeted<Protocol>>,
    pub file_associations: Vec<Targeted<FileAssociation>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Build {
    #[schemars(extend("minProperties" = 1))]
    pub targets: BTreeMap<TargetProfileId, TargetProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<ArtifactId, ArtifactProfile>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Universal,
    Single,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    Offline,
    Thin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProfile {
    #[serde(default = "default_artifact_kind")]
    pub kind: ArtifactKind,
    #[serde(default = "default_artifact_mode")]
    pub mode: ArtifactMode,
    pub targets: Vec<TargetProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

fn default_artifact_kind() -> ArtifactKind {
    ArtifactKind::Universal
}

fn default_artifact_mode() -> ArtifactMode {
    ArtifactMode::Offline
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    pub repository: String,
    pub channel: String,
    pub root: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    #[serde(default = "default_distribution_host")]
    pub host: DistributionHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DistributionHost {
    #[default]
    Static,
    Github,
}

impl DistributionHost {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Github => "github",
        }
    }
}

fn default_distribution_host() -> DistributionHost {
    DistributionHost::Static
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<Github>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Github {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<GithubWorkflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<GithubTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<GithubNotes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_tag: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_conflicts: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubTag {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubNotes {
    #[serde(default = "default_notes_policy")]
    pub policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

fn default_notes_policy() -> String {
    "generated".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubWorkflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attest_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_runner: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runners: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}
