//! Normalized installer IR.

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::model::{
    App, Component, FileAssociation, FileMapping, Frontend, Install, Launcher, PathEntry,
    PluginBinding, Protocol, Service,
};
use crate::prerequisite::Prerequisite;
use crate::target::TargetTriple;
use crate::{NonEmptyString, Sha256Digest};

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
    /// The preset this installer presents, as the runtime sees it.
    ///
    /// Absent only for a target that presents no window. Set by the build from a
    /// verified `.zupui`; nothing in this IR records where that package lived,
    /// because the runtime has no use for a build-machine fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<UiPreset>,
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

/// The preset an installer will present, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreset {
    /// The preset's own name, from its Cargo package.
    pub name: NonEmptyString,
    pub version: Version,
    /// The UI wire protocol the packaged preset speaks. The host checks it
    /// before launching rather than discovering it by failing to understand.
    pub protocol: u32,
    /// The capabilities this preset cannot present without.
    pub required_capabilities: Vec<String>,
    /// The application's settings, schema-validated against the package's own
    /// schema before this was composed.
    pub settings: serde_json::Value,
    /// The application-provided assets this preset's settings named, under the
    /// names the settings used. Their bytes live in the package's content store
    /// under `sha256`, so a preset never learns a source path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<UiAsset>,
}

/// One application-provided asset, identified by content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiAsset {
    /// The name the application's settings used.
    pub name: NonEmptyString,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// The UI runtime one installed application will present.
///
/// The same value in three places, deliberately: the plan a transaction will
/// commit, the journal that survives a crash, and the ledger that says what the
/// machine owns. A separate stored form would be a second model of the same
/// facts, and the two would eventually disagree about which settings a preset
/// receives.
///
/// The preset executable is addressed by content rather than by a role. In a
/// composed installer it is a resource with a fixed identifier, which is a fact
/// about composition; an installation that has outlived its installer has only
/// bytes, and a digest is the one name for bytes that both dedup and verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRuntime {
    /// The preset, as the runtime model: identity, protocol, capabilities,
    /// settings validated when the application was built, and the assets its
    /// settings named.
    pub preset: UiPreset,
    /// The preset executable this installation will launch.
    pub executable: Sha256Digest,
}

/// Runtime update trust configuration embedded by the build step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub repository: String,
    pub channel: String,
    pub trusted_root: Vec<u8>,
}
