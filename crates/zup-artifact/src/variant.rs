//! Distribution variants: one fully resolved native target.
//!
//! A variant is what a [`TargetProfile`](zup_core::TargetProfile) becomes once
//! its payload, plugins, prerequisites, and runtime template are resolved. It
//! is not an installer and it is not a file: it is the resolved content graph
//! for one machine, which an artifact may reference, ignore, or select.
//!
//! The split is deliberate. `DistributionVariant` holds the portable content
//! graph plus the build-machine inputs it was produced from, exactly as
//! `zup_build::ResolvedFile` holds a source path beside its portable identity.
//! Nothing build-machine-specific is ever serialized.

use std::collections::BTreeMap;
use std::path::PathBuf;

use semver::Version;
use serde::{Deserialize, Serialize};
use zup_bundle::{CompiledPluginArtifact, PayloadEntry, PluginArtifact, PortableBuildPlan};
use zup_core::{
    App, Frontend, Install, PluginId, PrerequisiteId, Sha256Digest, TargetProfileId, TargetTriple,
};

use crate::compat::frontend_subsystem;
use crate::descriptor::Descriptor;
use crate::error::ArtifactError;
use crate::media_type::MediaType;
use crate::platform::Platform;

/// Minimum host version one variant needs, when the target names one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimumHost {
    /// Operating system the requirement applies to.
    pub os: PlatformOs,
    /// Inclusive minimum version, as a dotted numeric tuple.
    pub version: HostVersion,
}

/// The operating system a minimum-host requirement applies to.
///
/// This is a wire value matched against the canonical operating system name in
/// a [`Platform`](crate::platform::Platform), so it names the systems the model
/// reasons about and stays open for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformOs {
    Windows,
    Linux,
    Macos,
    Ios,
    Android,
    Wasi,
    Other,
}

impl PlatformOs {
    /// The canonical triple spelling of this operating system.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Ios => "ios",
            Self::Android => "android",
            Self::Wasi => "wasi",
            Self::Other => "",
        }
    }

    /// The operating system this name denotes in a canonical triple.
    pub fn from_name(name: &str) -> Self {
        match name {
            "windows" => Self::Windows,
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "ios" => Self::Ios,
            "android" => Self::Android,
            "wasi" => Self::Wasi,
            _ => Self::Other,
        }
    }
}

/// A dotted numeric version with a bounded component count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl HostVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Whether this version is at least `other`.
    pub const fn at_least(self, other: &Self) -> bool {
        self.major > other.major
            || (self.major == other.major && self.minor > other.minor)
            || (self.major == other.major && self.minor == other.minor && self.patch >= other.patch)
    }
}

impl std::fmt::Display for HostVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One host-level capability a variant's installation may depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformCapability {
    /// Machine-wide components whose behavior cannot be emulated, such as
    /// drivers, native service binaries, or native integration points. A
    /// variant declaring this capability will not be selected through an
    /// emulation layer.
    MachineComponents,
}

impl PlatformCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MachineComponents => "machine_components",
        }
    }
}

/// What a variant needs from the host that runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRequirements {
    /// The variant must execute natively. A host that would need a
    /// compatibility or emulation layer refuses it.
    #[serde(default)]
    pub native_execution: bool,
    /// Host capabilities the installation depends on. Each one forbids an
    /// emulated fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<PlatformCapability>,
    /// Minimum host version, when the target names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_host: Option<MinimumHost>,
}

impl VariantRequirements {
    /// Whether an emulated execution of this variant would be sound.
    pub fn permits_emulation(&self) -> bool {
        !self.native_execution && self.capabilities.is_empty()
    }
}

/// Build-machine inputs for one variant, keyed by content digest.
///
/// The composer reads each unique digest exactly once through this map, so
/// identical payload content shared by several variants is hashed, read, and
/// compressed once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantSources {
    files: BTreeMap<Sha256Digest, PathBuf>,
    memory: BTreeMap<Sha256Digest, Vec<u8>>,
}

impl VariantSources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a file on the build machine that holds content with `digest`.
    pub fn with_file(mut self, digest: Sha256Digest, path: PathBuf) -> Self {
        self.files.insert(digest, path);
        self
    }

    /// Record in-memory content with `digest`, such as a compiled plugin.
    pub fn with_bytes(mut self, digest: Sha256Digest, bytes: Vec<u8>) -> Self {
        self.memory.insert(digest, bytes);
        self
    }

    /// Borrow a build-machine file for `digest`.
    pub fn file(&self, digest: &Sha256Digest) -> Option<&PathBuf> {
        self.files.get(digest)
    }

    /// Borrow in-memory content for `digest`.
    pub fn bytes(&self, digest: &Sha256Digest) -> Option<&[u8]> {
        self.memory.get(digest).map(Vec::as_slice)
    }

    /// Every digest this variant's content consists of, in ascending order.
    pub fn digests(&self) -> Vec<Sha256Digest> {
        let mut digests: Vec<Sha256Digest> = self
            .files
            .keys()
            .chain(self.memory.keys())
            .copied()
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }
}

/// One resolved native target, ready to compose into artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributionVariant {
    id: String,
    profile: TargetProfileId,
    target: TargetTriple,
    platform: Platform,
    frontend: Frontend,
    install: Install,
    application: App,
    plan: PortableBuildPlan,
    requirements: VariantRequirements,
    runtime: Option<Descriptor>,
    sources: VariantSources,
    logical_size: u64,
}

impl DistributionVariant {
    /// Resolve one target profile into a variant from a materialized build plan
    /// and its compiled plugins.
    ///
    /// `runtime` is the native maintenance runtime image for this target. It is
    /// required for an offline universal artifact, where a selected variant has
    /// to be executable without anything else present.
    pub fn resolve(
        config: &zup_core::ResolvedTargetConfig,
        plan: &zup_build::TargetBuildPlan,
        plugins: &[CompiledPluginArtifact],
        runtime: Option<(MediaType, Vec<u8>)>,
    ) -> Result<Self, ArtifactError> {
        if plan.installer.target != config.target {
            return Err(ArtifactError::Invalid);
        }
        let mut sources = VariantSources::new();
        for file in &plan.files {
            sources = sources.with_file(file.sha256, file.source.clone());
        }
        for prerequisite in &plan.prerequisites {
            sources = sources.with_file(prerequisite.sha256, prerequisite.source.clone());
        }
        for artifact in plugins {
            let digest = artifact.metadata().blob;
            sources = sources.with_bytes(digest, artifact.bytes().to_vec());
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        for file in &plan.files {
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size: file.size,
                sha256: file.sha256,
                blob: file.sha256,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        // A variant manifest is canonical, so its payload entries are ordered the
        // way every package reader requires: by destination, then by path.
        entries.sort_by(|left, right| {
            left.destination
                .to_string()
                .cmp(&right.destination.to_string())
                .then(left.path.cmp(&right.path))
        });
        let mut prerequisite_artifacts = Vec::with_capacity(plan.prerequisites.len());
        for prerequisite in &plan.prerequisites {
            prerequisite_artifacts.push(zup_bundle::PrerequisiteArtifact {
                prerequisite_id: prerequisite.id.clone(),
                path: prerequisite.source_relative.clone(),
                size: prerequisite.size,
                sha256: prerequisite.sha256,
                blob: prerequisite.sha256,
            });
        }
        prerequisite_artifacts
            .sort_by(|left, right| left.prerequisite_id.cmp(&right.prerequisite_id));
        let mut plugin_artifacts: Vec<PluginArtifact> = plugins
            .iter()
            .map(|artifact| artifact.metadata().clone())
            .collect();
        plugin_artifacts.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        let portable = PortableBuildPlan {
            installer: plan.installer.clone(),
            entries,
            prerequisite_artifacts,
            plugins: plugin_artifacts,
            total_size: plan.total_size,
        };
        let runtime = match runtime {
            Some((media_type, bytes)) => {
                let descriptor = Descriptor::of(media_type, &bytes);
                // The runtime image is content the composer must be able to read
                // again, so it is registered beside the payload like every other
                // blob, keyed by its own digest.
                sources = sources.with_bytes(descriptor.digest, bytes);
                Some(descriptor)
            }
            None => None,
        };
        let logical_size = logical_size(&portable, runtime.as_ref());
        Ok(Self {
            id: config.profile.to_string(),
            profile: config.profile.clone(),
            target: config.target.clone(),
            platform: Platform::from_triple(&config.target),
            frontend: config.frontend,
            install: config.install.clone(),
            application: plan.installer.app.clone(),
            plan: portable,
            requirements: VariantRequirements::default(),
            runtime,
            sources,
            logical_size,
        })
    }

    /// Replace the derived requirements with explicit ones.
    pub fn with_requirements(mut self, requirements: VariantRequirements) -> Self {
        self.requirements = requirements;
        self
    }

    /// Override the portable variant id, which is the profile name by default.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Override the resolved install configuration.
    ///
    /// Composition compares this across variants, so a fixture that needs two
    /// variants to disagree can say so without building two manifests.
    pub fn with_install(mut self, install: Install) -> Self {
        self.plan.installer.install = install.clone();
        self.install = install;
        self
    }

    /// Override the application version the variant claims to install.
    pub fn with_version(mut self, version: &str) -> Self {
        let version = semver::Version::parse(version).expect("a fixture version parses");
        self.application.version = version.clone();
        self.plan.installer.app.version = version;
        self
    }

    /// A stable name for this variant inside an artifact.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn profile(&self) -> &TargetProfileId {
        &self.profile
    }

    pub fn target(&self) -> &TargetTriple {
        &self.target
    }

    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    pub fn frontend(&self) -> Frontend {
        self.frontend
    }

    /// The launcher subsystem class this variant's installer experience needs.
    pub fn subsystem(&self) -> crate::compat::LauncherSubsystem {
        frontend_subsystem(self.frontend)
    }

    pub fn install(&self) -> &Install {
        &self.install
    }

    pub fn application(&self) -> &App {
        &self.application
    }

    pub fn version(&self) -> &Version {
        &self.application.version
    }

    pub fn plan(&self) -> &PortableBuildPlan {
        &self.plan
    }

    pub fn requirements(&self) -> &VariantRequirements {
        &self.requirements
    }

    /// The native maintenance runtime image this variant executes as.
    pub fn runtime(&self) -> Option<&Descriptor> {
        self.runtime.as_ref()
    }

    pub fn sources(&self) -> &VariantSources {
        &self.sources
    }

    /// Sum of the variant's content sizes, counting shared content once per
    /// variant. This is what a standalone artifact of this variant would cost.
    pub fn logical_size(&self) -> u64 {
        self.logical_size
    }

    /// Every content digest this variant requires, in ascending order.
    pub fn content_digests(&self) -> Vec<Sha256Digest> {
        let mut digests: Vec<Sha256Digest> = self
            .plan
            .entries
            .iter()
            .map(|entry| entry.blob)
            .chain(
                self.plan
                    .prerequisite_artifacts
                    .iter()
                    .map(|artifact| artifact.blob),
            )
            .chain(self.plan.plugins.iter().map(|artifact| artifact.blob))
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }

    /// Plugin identifiers this variant binds, used to prove a selected
    /// variant loads only its own ahead-of-time plugins.
    pub fn plugin_ids(&self) -> Vec<PluginId> {
        self.plan
            .plugins
            .iter()
            .map(|artifact| artifact.plugin_id.clone())
            .collect()
    }

    /// Prerequisite identifiers this variant embeds.
    pub fn prerequisite_ids(&self) -> Vec<PrerequisiteId> {
        self.plan
            .prerequisite_artifacts
            .iter()
            .map(|artifact| artifact.prerequisite_id.clone())
            .collect()
    }
}

fn logical_size(plan: &PortableBuildPlan, runtime: Option<&Descriptor>) -> u64 {
    let content = plan
        .entries
        .iter()
        .try_fold(0u64, |sum, entry| sum.checked_add(entry.size))
        .and_then(|sum| {
            plan.prerequisite_artifacts
                .iter()
                .try_fold(sum, |sum, artifact| sum.checked_add(artifact.size))
        })
        .and_then(|sum| {
            plan.plugins
                .iter()
                .try_fold(sum, |sum, artifact| sum.checked_add(artifact.aot_size))
        })
        .unwrap_or(u64::MAX);
    match runtime {
        Some(runtime) => content.saturating_add(runtime.size),
        None => content,
    }
}

/// Content accounting a selector and a report can rely on without reading the
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantDescriptorContent {
    pub logical_size: u64,
    pub blob_count: u64,
    pub unique_blob_count: u64,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
}

/// The portable descriptor a variant contributes to an artifact index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantDescriptor {
    pub id: String,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: Frontend,
    pub manifest: Descriptor,
    pub requirements: VariantRequirements,
    pub runtime: Option<Descriptor>,
    pub content: VariantDescriptorContent,
    pub logical_size: u64,
}

/// The serialized content graph of one variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantManifest {
    pub schema: u32,
    pub required_features: u64,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: Frontend,
    pub runtime: Option<Descriptor>,
    pub requirements: VariantRequirements,
    pub plan: PortableBuildPlan,
    pub logical_size: u64,
}

/// Current variant manifest schema.
pub const VARIANT_MANIFEST_SCHEMA: u32 = 1;

impl VariantManifest {
    /// Serialize a variant's content graph canonically.
    pub fn encode(variant: &DistributionVariant) -> Result<Vec<u8>, ArtifactError> {
        let manifest = Self {
            schema: VARIANT_MANIFEST_SCHEMA,
            required_features: crate::index::FEATURE_VARIANT_MANIFESTS,
            target: variant.target.clone(),
            platform: variant.platform.clone(),
            frontend: variant.frontend,
            runtime: variant.runtime,
            requirements: variant.requirements.clone(),
            plan: variant.plan.clone(),
            logical_size: variant.logical_size,
        };
        crate::descriptor::to_canonical_json(&manifest)
    }

    /// Parse and structurally validate a variant manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let manifest: Self = crate::descriptor::from_bounded_json(
            bytes,
            crate::media_type::MAX_VARIANT_MANIFEST_BYTES,
            "variant manifest",
        )?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject a manifest whose identity fields disagree with each other.
    pub fn validate(&self) -> Result<(), ArtifactError> {
        crate::index::check_features(self.required_features, "variant manifest")?;
        if self.schema != VARIANT_MANIFEST_SCHEMA
            || self.platform != Platform::from_triple(&self.target)
            || self.plan.installer.target != self.target
            || self.plan.installer.frontend != self.frontend
        {
            return Err(ArtifactError::Invalid);
        }
        if self.frontend != self.plan.installer.frontend {
            return Err(ArtifactError::Invalid);
        }
        let computed = logical_size(&self.plan, self.runtime.as_ref());
        if computed != self.logical_size {
            return Err(ArtifactError::Invalid);
        }
        if let Some(runtime) = self.runtime
            && runtime.media_type != MediaType::RUNTIME
        {
            return Err(ArtifactError::Invalid);
        }
        Ok(())
    }

    /// Every content digest this variant requires, in ascending order.
    pub fn content_digests(&self) -> Vec<Sha256Digest> {
        let mut digests: Vec<Sha256Digest> = self
            .plan
            .entries
            .iter()
            .map(|entry| entry.blob)
            .chain(
                self.plan
                    .prerequisite_artifacts
                    .iter()
                    .map(|artifact| artifact.blob),
            )
            .chain(self.plan.plugins.iter().map(|artifact| artifact.blob))
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }
}
