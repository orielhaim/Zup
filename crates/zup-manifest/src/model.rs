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
/// A resource declaration with optional target-profile applicability.
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
/// Build configuration and target matrix.
pub struct Build {
    #[schemars(extend("minProperties" = 1))]
    pub targets: BTreeMap<TargetProfileId, TargetProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    /// Distribution artifacts this project publishes.
    ///
    /// Declaring artifacts is optional. A project that declares none builds one
    /// installer per selected target, which is the simplest thing that works.
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
/// How much of a target set one artifact file carries.
pub enum ArtifactKind {
    /// One file carrying every included target, selected at run time.
    Universal,
    /// One file carrying exactly one target.
    Single,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Whether an artifact carries its content or fetches it.
pub enum ArtifactMode {
    /// Every required byte is inside the artifact. No network is used.
    Offline,
    /// The artifact carries what it needs to start and select a variant; the rest
    /// is fetched by digest through the update trust configuration.
    Thin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// One declared distribution artifact.
pub struct ArtifactProfile {
    #[serde(default = "default_artifact_kind")]
    pub kind: ArtifactKind,
    #[serde(default = "default_artifact_mode")]
    pub mode: ArtifactMode,
    /// The target profiles this artifact includes. Required.
    pub targets: Vec<TargetProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The release channel this artifact follows.
    ///
    /// An artifact without a channel is labelled with an exact version and always
    /// installs it. An artifact with one installs the channel's current release,
    /// which is a different promise and a different file name.
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The output file name. Derived from the application name when absent.
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
/// Build-time update repository settings.
pub struct Updates {
    pub repository: String,
    pub channel: String,
    pub root: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Where a published release is hosted.
///
/// The section exists so "publish there" and "fetch from there" are separate
/// decisions. A project can publish installers to GitHub and still serve
/// content from a CDN it controls, or publish to both. The only thing this
/// section says is where a *client* looks.
pub struct Distribution {
    #[serde(default = "default_distribution_host")]
    /// Where a client fetches release content from.
    ///
    /// `static` is a plain file tree a CDN or web server serves, which is the
    /// default and needs no configuration. `github` is release assets, which is
    /// the zero-infrastructure option: no bucket, no CDN, no origin.
    pub host: DistributionHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The channel the `github` host's stable alias follows.
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Where a client fetches release content from.
pub enum DistributionHost {
    #[default]
    /// A plain directory of files: `blobs/`, `releases/`, `metadata/`.
    Static,
    /// Release assets, addressed by tag or by the host's stable alias.
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
/// Where a release is published.
pub struct Publish {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// GitHub Releases.
    pub github: Option<Github>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// GitHub Releases as a publish target.
///
/// There is deliberately no field for a credential. A manifest is committed and
/// shared, so a token in one reaches every fork of the project; the credential
/// comes from the environment or from `gh`, and this type cannot hold it.
pub struct Github {
    /// `owner/name`, or `host/owner/name` for GitHub Enterprise.
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The generated release workflow's settings.
    pub workflow: Option<GithubWorkflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// How a release's tag is spelled.
    pub tag: Option<GithubTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// What a release's notes say.
    pub notes: Option<GithubNotes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Leave the release a draft.
    pub draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Mark the release a prerelease.
    pub prerelease: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Let the publisher create the tag when it does not exist.
    ///
    /// Off by default, and the reason is specific rather than general: a
    /// provider that creates a tag from whatever its default branch points at
    /// will one day publish a release for code that was never built, and no
    /// amount of downstream verification puts that commit back.
    pub create_tag: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Replace a differing asset on a draft release.
    pub replace_conflicts: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// How a release's tag is spelled.
pub struct GithubTag {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// A prefix followed by the version. `v1.4.0` by default.
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// An exact tag, when the project does not derive one from the version.
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Where a release's notes come from.
pub struct GithubNotes {
    #[serde(default = "default_notes_policy")]
    /// `generated`, `file`, `text`, or `none`.
    pub policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The file to read, when the policy is `file`.
    pub file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The text to use, when the policy is `text`.
    pub text: Option<String>,
}

fn default_notes_policy() -> String {
    "generated".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// The generated release workflow's settings.
///
/// Every field here is a decision a project makes and the generator only
/// reflects. The *matrix* is not one of them: it comes from the resolved target
/// profiles, because a developer should not have to enumerate targets in YAML
/// and keep the two in sync.
pub struct GithubWorkflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The workflow's `name:`.
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The tag glob that triggers a release.
    pub tag_glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// A GitHub environment to gate the publish phase behind.
    ///
    /// Opt-in. Required reviewers, environment secrets, and tag restrictions
    /// are three clicks on an environment, and reimplementing any of them in
    /// zup would be a worse version of the same thing.
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Whether to generate build-provenance attestations.
    pub attestations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Extra globs to attest, relative to the release directory.
    ///
    /// Empty by default. Which files a user runs is the project's decision, so
    /// the generator does not guess it; the release manifest is attested either
    /// way, because it is the document that names every other digest.
    pub attest_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Commands that sign the composed artifacts.
    ///
    /// A list of lines rather than one string, because the generated workflow
    /// writes them into a `run:` block and a project needs to see the shape of
    /// what it is authorizing.
    pub sign: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The runner that composes the final artifacts.
    pub compose_runner: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    /// Runner labels the project pinned itself, by target triple.
    pub runners: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The directory the release is composed in.
    pub release_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Where the publisher writes its receipt.
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The zup action the generated pipeline calls, as `owner/repo@ref`.
    ///
    /// Defaults to a floating major ref, because that is what an action ref is
    /// for: a project writes it once and dependabot keeps it current. A project
    /// that wants the release pipeline pinned to an immutable ref names one here
    /// instead, and the generated file shows exactly what it will run.
    pub action: Option<String>,
}
