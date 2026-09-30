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
    Service, TargetProfile, TargetProfileId, Ui,
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
    pub ui: Ui,
    pub build: Build,
    pub install: Install,
    pub prerequisites: Vec<Targeted<Prerequisite>>,
    pub updates: Option<Updates>,
    /// Where a published release is hosted.
    pub distribution: Option<Distribution>,
    /// Where a release is published.
    pub publish: Option<Publish>,
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

/// Where a published release is hosted.
///
/// The section exists so "publish there" and "fetch from there" are separate
/// decisions. A project can publish installers to GitHub and still serve
/// content from a CDN it controls, or publish to both. The only thing this
/// section says is where a *client* looks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    /// Where a client fetches release content from.
    ///
    /// `static` is a plain file tree a CDN or web server serves, which is the
    /// default and needs no configuration. `github` is release assets, which is
    /// the zero-infrastructure option: no bucket, no CDN, no origin.
    #[serde(default = "default_distribution_host")]
    pub host: DistributionHost,
    /// The channel the `github` host's stable alias follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// Where a client fetches release content from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DistributionHost {
    /// A plain directory of files: `blobs/`, `releases/`, `metadata/`.
    #[default]
    Static,
    /// Release assets, addressed by tag or by the host's stable alias.
    Github,
}

impl DistributionHost {
    /// The name a report and a diagnostic use.
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

/// Where a release is published.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    /// GitHub Releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<Github>,
}

/// GitHub Releases as a publish target.
///
/// There is deliberately no field for a credential. A manifest is committed and
/// shared, so a token in one reaches every fork of the project; the credential
/// comes from the environment or from `gh`, and this type cannot hold it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Github {
    /// `owner/name`, or `host/owner/name` for GitHub Enterprise.
    pub repository: String,
    /// The generated release workflow's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<GithubWorkflow>,
    /// How a release's tag is spelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<GithubTag>,
    /// What a release's notes say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<GithubNotes>,
    /// Leave the release a draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    /// Mark the release a prerelease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<bool>,
    /// Let the publisher create the tag when it does not exist.
    ///
    /// Off by default, and the reason is specific rather than general: a
    /// provider that creates a tag from whatever its default branch points at
    /// will one day publish a release for code that was never built, and no
    /// amount of downstream verification puts that commit back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_tag: Option<bool>,
    /// Replace a differing asset on a draft release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_conflicts: Option<bool>,
}

/// How a release's tag is spelled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubTag {
    /// A prefix followed by the version. `v1.4.0` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// An exact tag, when the project does not derive one from the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where a release's notes come from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubNotes {
    /// `generated`, `file`, `text`, or `none`.
    #[serde(default = "default_notes_policy")]
    pub policy: String,
    /// The file to read, when the policy is `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<std::path::PathBuf>,
    /// The text to use, when the policy is `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

fn default_notes_policy() -> String {
    "generated".to_owned()
}

/// The generated release workflow's settings.
///
/// Every field here is a decision a project makes and the generator only
/// reflects. The *matrix* is not one of them: it comes from the resolved target
/// profiles, because a developer should not have to enumerate targets in YAML
/// and keep the two in sync.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubWorkflow {
    /// The workflow's `name:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The tag glob that triggers a release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_glob: Option<String>,
    /// A GitHub environment to gate the publish phase behind.
    ///
    /// Opt-in. Required reviewers, environment secrets, and tag restrictions
    /// are three clicks on an environment, and reimplementing any of them in
    /// zup would be a worse version of the same thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Whether to generate build-provenance attestations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestations: Option<bool>,
    /// Extra globs to attest, relative to the release directory.
    ///
    /// Empty by default. Which files a user runs is the project's decision, so
    /// the generator does not guess it; the release manifest is attested either
    /// way, because it is the document that names every other digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attest_paths: Option<Vec<String>>,
    /// Commands that sign the composed artifacts.
    ///
    /// A list of lines rather than one string, because the generated workflow
    /// writes them into a `run:` block and a project needs to see the shape of
    /// what it is authorizing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign: Option<Vec<String>>,
    /// The runner that composes the final artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_runner: Option<String>,
    /// Runner labels the project pinned itself, by target triple.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runners: BTreeMap<String, String>,
    /// The directory the release is composed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_dir: Option<String>,
    /// Where the publisher writes its receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    /// The zup action the generated pipeline calls, as `owner/repo@ref`.
    ///
    /// Defaults to a floating major ref, because that is what an action ref is
    /// for: a project writes it once and dependabot keeps it current. A project
    /// that wants the release pipeline pinned to an immutable ref names one here
    /// instead, and the generated file shows exactly what it will run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}
