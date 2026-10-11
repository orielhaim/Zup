use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_acquire::{OnlineTrust, ReleasePin};
use zup_core::{App, Sha256Digest, TargetTriple};

use crate::compat::LauncherSubsystem;
use crate::format::ArtifactError;
use crate::format::Descriptor;
use crate::format::{MAX_INDEX_BYTES, MAX_VARIANTS, MediaType};
use crate::platform::Platform;
use crate::variant::{VariantDescriptor, VariantRequirements};

pub const ARTIFACT_SCHEMA: u32 = 1;

pub const FEATURE_SHARED_CAS: u64 = 1 << 0;
pub const FEATURE_VARIANT_MANIFESTS: u64 = 1 << 1;
pub const FEATURE_CHANNEL_PIN: u64 = 1 << 2;
pub const SUPPORTED_FEATURES: u64 =
    FEATURE_SHARED_CAS | FEATURE_VARIANT_MANIFESTS | FEATURE_CHANNEL_PIN;

pub fn check_features(required: u64, kind: &'static str) -> Result<(), ArtifactError> {
    let unknown = required & !SUPPORTED_FEATURES;
    if unknown != 0 {
        return Err(ArtifactError::UnsupportedFeatures {
            kind,
            unknown,
            supported: SUPPORTED_FEATURES,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Universal,
    Single,
}

impl ArtifactKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Universal => "universal",
            Self::Single => "single",
        }
    }

    pub const fn variant_capacity(self) -> usize {
        match self {
            Self::Universal => MAX_VARIANTS,
            Self::Single => 1,
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    Offline,
    Thin,
}

impl ArtifactMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Thin => "thin",
        }
    }

    pub const fn carries_content(self) -> bool {
        matches!(self, Self::Offline)
    }
}

impl std::fmt::Display for ArtifactMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPin {
    Pinned {
        version: semver::Version,
    },
    /// configuration. A version-labelled artifact is never this.
    Channel {
        channel: String,
    },
}

impl ArtifactPin {
    pub fn label(&self) -> String {
        match self {
            Self::Pinned { version } => version.to_string(),
            Self::Channel { channel } => channel.clone(),
        }
    }

    pub fn slug(&self) -> String {
        match self {
            Self::Pinned { version } => version.to_string(),
            Self::Channel { channel } => channel.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LauncherStrategy {
    EmbeddedDispatcher,
    HostSelectedContainer,
}

impl LauncherStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmbeddedDispatcher => "embedded-dispatcher",
            Self::HostSelectedContainer => "host-selected-container",
        }
    }

    pub const fn requires_shared_subsystem(self) -> bool {
        matches!(self, Self::EmbeddedDispatcher)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub id: String,
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    pub pin: ArtifactPin,
    pub application: App,
    pub launcher: LauncherStrategy,
    pub subsystem: LauncherSubsystem,
    pub output: String,
    /// artifact leaves it `None` and never looks at a network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<OnlineTrust>,
}

impl ArtifactDescriptor {
    pub fn is_version_labelled(&self) -> bool {
        matches!(self.pin, ArtifactPin::Pinned { .. })
    }

    /// artifact never will.
    pub const fn is_offline(&self) -> bool {
        matches!(self.mode, ArtifactMode::Offline)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match (&self.mode, &self.trust) {
            (ArtifactMode::Offline, Some(_)) => {
                Err("an offline artifact carries no online trust block")
            }
            (ArtifactMode::Thin, None) => Err("a thin artifact embeds no trust block"),
            (ArtifactMode::Thin, Some(trust)) => {
                if !trust.channel.is_empty() && self.application.id != trust.app_id {
                    return Err("a trust block names a different application");
                }
                match (&self.pin, &trust.pin) {
                    (ArtifactPin::Pinned { .. }, ReleasePin::Channel { .. }) => {
                        Err("a version-labelled artifact needs a pinned trust block")
                    }
                    (ArtifactPin::Channel { .. }, ReleasePin::Version { .. }) => {
                        Err("a channel artifact needs an unpinned trust block")
                    }
                    (ArtifactPin::Pinned { version }, ReleasePin::Version { version: pinned }) => {
                        if version.to_string() != *pinned {
                            return Err(
                                "a version-labelled artifact and its trust block disagree on the version",
                            );
                        }
                        Ok(())
                    }
                    (ArtifactPin::Channel { channel }, ReleasePin::Channel { channel: pinned }) => {
                        if channel != pinned {
                            return Err(
                                "a channel artifact and its trust block disagree on the channel",
                            );
                        }
                        Ok(())
                    }
                }
            }
            (ArtifactMode::Offline, None) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTables {
    pub blobs: Descriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIndex {
    pub schema: u32,
    pub required_features: u64,
    pub media_type: MediaType,
    pub artifact: ArtifactDescriptor,
    pub tables: ArtifactTables,
    /// Variants sorted by id, so the file order of the graph never changes a
    pub variants: Vec<VariantDescriptor>,
}

impl ArtifactIndex {
    pub fn encode(&self) -> Result<Vec<u8>, ArtifactError> {
        crate::format::to_canonical_json(self)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let index: Self =
            crate::format::from_bounded_json(bytes, MAX_INDEX_BYTES, "artifact index")?;
        index.validate()?;
        Ok(index)
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        check_features(self.required_features, "artifact index")?;
        if self.schema != ARTIFACT_SCHEMA
            || self.media_type != MediaType::INDEX
            || self.tables.blobs.media_type != MediaType::BLOB_TABLE
            || self.variants.is_empty()
            || self.variants.len() > self.artifact.kind.variant_capacity()
        {
            return Err(ArtifactError::Invalid);
        }
        if self.artifact.output.is_empty()
            || self.artifact.output.contains('/')
            || self.artifact.output.contains('\\')
            || self.artifact.id.is_empty()
        {
            return Err(ArtifactError::Invalid);
        }
        if self.artifact.application.id.as_str().is_empty() {
            return Err(ArtifactError::Invalid);
        }
        for (index, variant) in self.variants.iter().enumerate() {
            self.validate_variant(variant)?;
            if index > 0 && self.variants[index - 1].id >= variant.id {
                return Err(ArtifactError::Invalid);
            }
        }
        Ok(())
    }

    fn validate_variant(&self, variant: &VariantDescriptor) -> Result<(), ArtifactError> {
        if variant.id.is_empty()
            || variant.manifest.media_type != MediaType::VARIANT_MANIFEST
            || variant.platform != Platform::from_triple(&variant.target)
            || variant.subsystem() != self.artifact.subsystem
            || variant.content.blob_count > variant.content.unique_blob_count
            || variant.content.logical_size > variant.logical_size
        {
            return Err(ArtifactError::Invalid);
        }
        if variant
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.media_type != MediaType::RUNTIME)
        {
            return Err(ArtifactError::Invalid);
        }
        Ok(())
    }

    pub fn variant(&self, id: &str) -> Option<&VariantDescriptor> {
        self.variants.iter().find(|variant| variant.id == id)
    }

    pub fn variant_ids(&self) -> Vec<&str> {
        self.variants
            .iter()
            .map(|variant| variant.id.as_str())
            .collect()
    }

    pub fn carries_every_runtime(&self) -> bool {
        self.variants
            .iter()
            .all(|variant| variant.runtime.is_some())
    }

    pub fn standalone_size(&self) -> u64 {
        self.variants
            .iter()
            .try_fold(0u64, |sum, variant| sum.checked_add(variant.logical_size))
            .unwrap_or(u64::MAX)
    }
}

trait VariantSubsystem {
    fn subsystem(&self) -> LauncherSubsystem;
}

impl VariantSubsystem for VariantDescriptor {
    fn subsystem(&self) -> LauncherSubsystem {
        crate::compat::frontend_subsystem(self.frontend)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSavings {
    pub standalone_size: u64,
    pub unique_variant_size: u64,
    pub shared_size: u64,
    pub exclusive_size: u64,
    pub unique_blob_count: u64,
}

impl ArtifactSavings {
    pub fn deduplicated(&self) -> u64 {
        self.standalone_size
            .saturating_sub(self.unique_variant_size)
    }
}

/// the same immutable location and a client never has to be told where to look
pub fn remote_blob_path(digest: &Sha256Digest) -> String {
    format!("blobs/sha256/{}", digest.to_hex())
}

#[derive(Debug, Clone)]
pub struct VariantContentSet {
    pub id: String,
    pub digests: Vec<Sha256Digest>,
    pub logical_size: u64,
    pub unique_blob_count: u64,
}

pub fn savings(sets: &[VariantContentSet], table: &crate::table::BlobTable) -> ArtifactSavings {
    let mut counts: BTreeMap<Sha256Digest, usize> = BTreeMap::new();
    for set in sets {
        for digest in &set.digests {
            *counts.entry(*digest).or_default() += 1;
        }
    }
    let mut shared_size = 0u64;
    let mut exclusive_size = 0u64;
    for (digest, count) in &counts {
        let size = table.entry(digest).map_or(0, |entry| entry.size);
        if *count > 1 {
            shared_size = shared_size.saturating_add(size);
        } else {
            exclusive_size = exclusive_size.saturating_add(size);
        }
    }
    ArtifactSavings {
        standalone_size: sets
            .iter()
            .try_fold(0u64, |sum, set| sum.checked_add(set.logical_size))
            .unwrap_or(u64::MAX),
        unique_variant_size: counts
            .iter()
            .try_fold(0u64, |sum, (digest, _)| {
                sum.checked_add(table.entry(digest).map_or(0, |entry| entry.size))
            })
            .unwrap_or(u64::MAX),
        shared_size,
        exclusive_size,
        unique_blob_count: counts.len() as u64,
    }
}

pub fn covered_targets(index: &ArtifactIndex) -> Vec<TargetTriple> {
    index
        .variants
        .iter()
        .map(|variant| variant.target.clone())
        .collect()
}

pub fn shared_requirements(index: &ArtifactIndex) -> VariantRequirements {
    let native_execution = index
        .variants
        .iter()
        .all(|variant| variant.requirements.native_execution);
    VariantRequirements {
        native_execution,
        capabilities: Vec::new(),
        minimum_host: None,
    }
}
