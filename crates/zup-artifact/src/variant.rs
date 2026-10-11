use std::collections::BTreeMap;
use std::path::PathBuf;

use semver::Version;
use serde::{Deserialize, Serialize};
use zup_bundle::{CompiledPluginArtifact, PayloadEntry, PluginArtifact, PortableBuildPlan};
use zup_core::{
    App, Frontend, Install, PluginId, PrerequisiteId, Sha256Digest, TargetProfileId, TargetTriple,
};

use crate::compat::frontend_subsystem;
use crate::format::ArtifactError;
use crate::format::Descriptor;
use crate::format::MediaType;
use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimumHost {
    pub os: PlatformOs,
    pub version: HostVersion,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformCapability {
    MachineComponents,
}

impl PlatformCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MachineComponents => "machine_components",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRequirements {
    #[serde(default)]
    pub native_execution: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<PlatformCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_host: Option<MinimumHost>,
}

impl VariantRequirements {
    pub fn permits_emulation(&self) -> bool {
        !self.native_execution && self.capabilities.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantSources {
    files: BTreeMap<Sha256Digest, PathBuf>,
    memory: BTreeMap<Sha256Digest, Vec<u8>>,
}

impl VariantSources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, digest: Sha256Digest, path: PathBuf) -> Self {
        self.files.insert(digest, path);
        self
    }

    pub fn with_bytes(mut self, digest: Sha256Digest, bytes: Vec<u8>) -> Self {
        self.memory.insert(digest, bytes);
        self
    }

    pub fn file(&self, digest: &Sha256Digest) -> Option<&PathBuf> {
        self.files.get(digest)
    }

    pub fn bytes(&self, digest: &Sha256Digest) -> Option<&[u8]> {
        self.memory.get(digest).map(Vec::as_slice)
    }

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
    preset: Option<Descriptor>,
    sources: VariantSources,
    logical_size: u64,
}

impl DistributionVariant {
    pub fn resolve(
        config: &zup_core::ResolvedTargetConfig,
        plan: &zup_core::TargetBuildPlan,
        plugins: &[CompiledPluginArtifact],
        natives: &[(MediaType, Vec<u8>)],
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
        for asset in &plan.ui_assets {
            let Some(source) = asset.source.clone() else {
                return Err(ArtifactError::Invalid);
            };
            sources = sources.with_file(asset.sha256, source);
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
                executable: file.executable,
            });
        }
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
            ui_assets: plan
                .ui_assets
                .iter()
                .map(|asset| zup_core::PresetAsset {
                    name: asset.name.clone(),
                    size: asset.size,
                    sha256: asset.sha256,
                })
                .collect(),
            plugins: plugin_artifacts,
            total_size: plan.total_size,
        };
        let mut runtime = None;
        let mut preset = None;
        for (media_type, bytes) in natives {
            let descriptor = Descriptor::of(*media_type, bytes);
            sources = sources.with_bytes(descriptor.digest, bytes.clone());
            match media_type {
                MediaType::Runtime => runtime = Some(descriptor),
                MediaType::Preset => preset = Some(descriptor),
                other => {
                    let _ = other;
                    return Err(ArtifactError::Invalid);
                }
            }
        }
        let logical_size = logical_size(&portable, runtime.as_ref(), preset.as_ref());
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
            preset,
            sources,
            logical_size,
        })
    }

    pub fn with_requirements(mut self, requirements: VariantRequirements) -> Self {
        self.requirements = requirements;
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_install(mut self, install: Install) -> Self {
        self.plan.installer.install = install.clone();
        self.install = install;
        self
    }

    pub fn with_version(mut self, version: &str) -> Self {
        let version = semver::Version::parse(version).expect("a fixture version parses");
        self.application.version = version.clone();
        self.plan.installer.app.version = version;
        self
    }

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

    pub fn runtime(&self) -> Option<&Descriptor> {
        self.runtime.as_ref()
    }

    pub fn preset(&self) -> Option<&Descriptor> {
        self.preset.as_ref()
    }

    pub fn sources(&self) -> &VariantSources {
        &self.sources
    }

    pub fn logical_size(&self) -> u64 {
        self.logical_size
    }

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
            .chain(self.plan.ui_assets.iter().map(|asset| asset.sha256))
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }

    pub fn plugin_ids(&self) -> Vec<PluginId> {
        self.plan
            .plugins
            .iter()
            .map(|artifact| artifact.plugin_id.clone())
            .collect()
    }

    pub fn prerequisite_ids(&self) -> Vec<PrerequisiteId> {
        self.plan
            .prerequisite_artifacts
            .iter()
            .map(|artifact| artifact.prerequisite_id.clone())
            .collect()
    }
}

fn logical_size(
    plan: &PortableBuildPlan,
    runtime: Option<&Descriptor>,
    preset: Option<&Descriptor>,
) -> u64 {
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

    runtime
        .into_iter()
        .chain(preset)
        .fold(content, |sum, native| sum.saturating_add(native.size))
}

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
    pub preset: Option<Descriptor>,
    pub content: VariantDescriptorContent,
    pub logical_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantManifest {
    pub schema: u32,
    pub required_features: u64,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: Frontend,
    pub runtime: Option<Descriptor>,
    pub preset: Option<Descriptor>,
    pub requirements: VariantRequirements,
    pub plan: PortableBuildPlan,
    pub logical_size: u64,
}

pub const VARIANT_MANIFEST_SCHEMA: u32 = 1;

impl VariantManifest {
    pub fn encode(variant: &DistributionVariant) -> Result<Vec<u8>, ArtifactError> {
        let manifest = Self {
            schema: VARIANT_MANIFEST_SCHEMA,
            required_features: crate::index::FEATURE_VARIANT_MANIFESTS,
            target: variant.target.clone(),
            platform: variant.platform.clone(),
            frontend: variant.frontend,
            runtime: variant.runtime,
            preset: variant.preset,
            requirements: variant.requirements.clone(),
            plan: variant.plan.clone(),
            logical_size: variant.logical_size,
        };
        crate::format::to_canonical_json(&manifest)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let manifest: Self = crate::format::from_bounded_json(
            bytes,
            crate::format::MAX_VARIANT_MANIFEST_BYTES,
            "variant manifest",
        )?;
        manifest.validate()?;
        Ok(manifest)
    }

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
        let computed = logical_size(&self.plan, self.runtime.as_ref(), self.preset.as_ref());
        if computed != self.logical_size {
            return Err(ArtifactError::Invalid);
        }
        if let Some(runtime) = self.runtime
            && runtime.media_type != MediaType::RUNTIME
        {
            return Err(ArtifactError::Invalid);
        }
        if let Some(preset) = self.preset
            && preset.media_type != MediaType::PRESET
        {
            return Err(ArtifactError::Invalid);
        }
        Ok(())
    }

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
            .chain(self.plan.ui_assets.iter().map(|asset| asset.sha256))
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }
}
