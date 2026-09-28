//! The artifact index: the root of the graph.
//!
//! An index is small on purpose. It names the artifact, names the variants, and
//! points at immutable content through digests. It never inlines a variant
//! manifest, a blob table, or payload content, so a machine can read and
//! validate it before deciding whether the artifact applies to it at all.
//!
//! Selection reads the index. It never reads a filename, and it never infers a
//! platform from an ordering convention.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_acquire::{OnlineTrust, ReleasePin};
use zup_core::{App, Sha256Digest, TargetTriple};

use crate::compat::LauncherSubsystem;
use crate::descriptor::Descriptor;
use crate::error::ArtifactError;
use crate::media_type::{MAX_INDEX_BYTES, MAX_VARIANTS, MediaType};
use crate::platform::Platform;
use crate::variant::{VariantDescriptor, VariantRequirements};

/// Current artifact index schema.
pub const ARTIFACT_SCHEMA: u32 = 1;

/// The shared content store is addressed by one table.
pub const FEATURE_SHARED_CAS: u64 = 1 << 0;
/// Variants reference their content graph through manifests.
pub const FEATURE_VARIANT_MANIFESTS: u64 = 1 << 1;
/// An artifact may be pinned to a version or to a release channel.
pub const FEATURE_CHANNEL_PIN: u64 = 1 << 2;
/// Every feature this build understands.
pub const SUPPORTED_FEATURES: u64 =
    FEATURE_SHARED_CAS | FEATURE_VARIANT_MANIFESTS | FEATURE_CHANNEL_PIN;

/// Refuse a document that requires a feature this build does not implement.
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

/// What kind of thing a user downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// One file carrying every included variant.
    Universal,
    /// One file carrying exactly one variant.
    Single,
}

impl ArtifactKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Universal => "universal",
            Self::Single => "single",
        }
    }

    /// How many variants this kind may carry.
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

/// Whether the artifact carries its content or fetches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    /// Every required byte is inside the artifact. No network is used.
    Offline,
    /// The artifact carries what it needs to start, authenticate a release,
    /// and select a variant. The rest is fetched by digest and verified.
    Thin,
}

impl ArtifactMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Thin => "thin",
        }
    }

    /// Whether content bytes are expected inside the artifact.
    pub const fn carries_content(self) -> bool {
        matches!(self, Self::Offline)
    }
}

impl std::fmt::Display for ArtifactMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which release an artifact installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPin {
    /// Always installs one exact version.
    Pinned { version: semver::Version },
    /// Resolves the current release of a channel through the update trust
    /// configuration. A version-labelled artifact is never this.
    Channel { channel: String },
}

impl ArtifactPin {
    /// A short description for reports and derived filenames.
    pub fn label(&self) -> String {
        match self {
            Self::Pinned { version } => version.to_string(),
            Self::Channel { channel } => channel.clone(),
        }
    }

    /// A filename-safe token for this pin.
    pub fn slug(&self) -> String {
        match self {
            Self::Pinned { version } => version.to_string(),
            Self::Channel { channel } => channel.clone(),
        }
    }
}

/// How a platform backend reaches a selected variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LauncherStrategy {
    /// A small program inside the artifact inspects the host, selects a
    /// variant, materializes it, and starts it.
    EmbeddedDispatcher,
    /// The host's own loader selects the right content, as a Mach-O universal
    /// binary or a package bundle does.
    HostSelectedContainer,
}

impl LauncherStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmbeddedDispatcher => "embedded-dispatcher",
            Self::HostSelectedContainer => "host-selected-container",
        }
    }

    /// Whether the strategy requires the variants to agree on a launcher
    /// subsystem, which every embedded launcher has.
    pub const fn requires_shared_subsystem(self) -> bool {
        matches!(self, Self::EmbeddedDispatcher)
    }
}

/// What a user gets, and what a machine reads.
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
    /// Filename the build writes, without a directory.
    pub output: String,
    /// Where the release graph lives and which root vouches for it.
    ///
    /// A thin artifact cannot be small unless it already knows this, so it is
    /// the one thing a thin index carries beyond the graph itself. An offline
    /// artifact leaves it `None` and never looks at a network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<OnlineTrust>,
}

impl ArtifactDescriptor {
    /// Whether the artifact is labelled with an exact version.
    pub fn is_version_labelled(&self) -> bool {
        matches!(self.pin, ArtifactPin::Pinned { .. })
    }

    /// Whether this artifact can install with no network.
    ///
    /// The answer is about the artifact, not about the host: a thin artifact
    /// with a `--source` seed and a warm cache may not need one, and an offline
    /// artifact never will.
    pub const fn is_offline(&self) -> bool {
        matches!(self.mode, ArtifactMode::Offline)
    }

    /// Reject a descriptor whose own claims disagree.
    ///
    /// A thin artifact with no trust block cannot resolve anything, and an
    /// offline artifact carrying one is a sign a build wired the wrong request -
    /// neither is worth discovering on a user's machine.
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

/// The immutable content locations an index points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTables {
    pub blobs: Descriptor,
}

/// The root of a distribution artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIndex {
    pub schema: u32,
    pub required_features: u64,
    pub media_type: MediaType,
    pub artifact: ArtifactDescriptor,
    pub tables: ArtifactTables,
    /// Variants sorted by id, so the file order of the graph never changes a
    /// selection.
    pub variants: Vec<VariantDescriptor>,
}

impl ArtifactIndex {
    /// Serialize canonically.
    pub fn encode(&self) -> Result<Vec<u8>, ArtifactError> {
        crate::descriptor::to_canonical_json(self)
    }

    /// Parse and validate an index, failing closed on anything a selector could
    /// not trust.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let index: Self =
            crate::descriptor::from_bounded_json(bytes, MAX_INDEX_BYTES, "artifact index")?;
        index.validate()?;
        Ok(index)
    }

    /// Reject an index whose structure a selector could not rely on.
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

    /// The variant descriptor with this id.
    pub fn variant(&self, id: &str) -> Option<&VariantDescriptor> {
        self.variants.iter().find(|variant| variant.id == id)
    }

    /// The variants of this index that carry bytes, which is every variant in
    /// an offline artifact and none in a thin one that only carries runtimes.
    pub fn variant_ids(&self) -> Vec<&str> {
        self.variants
            .iter()
            .map(|variant| variant.id.as_str())
            .collect()
    }

    /// Whether every variant declares a native runtime, which a thin artifact
    /// requires so it can start without fetching anything to run on.
    pub fn carries_every_runtime(&self) -> bool {
        self.variants
            .iter()
            .all(|variant| variant.runtime.is_some())
    }

    /// Sum of the variants' logical sizes, which is what separate artifacts
    /// would cost.
    pub fn standalone_size(&self) -> u64 {
        self.variants
            .iter()
            .try_fold(0u64, |sum, variant| sum.checked_add(variant.logical_size))
            .unwrap_or(u64::MAX)
    }
}

/// Extension trait so index validation can ask a variant for its subsystem
/// without importing the compatibility model into the index module.
trait VariantSubsystem {
    fn subsystem(&self) -> LauncherSubsystem;
}

impl VariantSubsystem for VariantDescriptor {
    fn subsystem(&self) -> LauncherSubsystem {
        crate::compat::frontend_subsystem(self.frontend)
    }
}

/// The account of what an artifact saved, which is what makes the value of
/// composing it visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSavings {
    /// What the variants would cost as separate artifacts: each one's logical
    /// size, so shared content is counted once per variant that needs it.
    pub standalone_size: u64,
    /// What the composed artifact's content costs: every distinct blob once.
    pub unique_variant_size: u64,
    /// Bytes of content more than one variant needs.
    pub shared_size: u64,
    /// Bytes of content exactly one variant needs.
    pub exclusive_size: u64,
    /// Distinct blobs the artifact carries.
    pub unique_blob_count: u64,
}

impl ArtifactSavings {
    /// Bytes composing saved, against separate artifacts.
    pub fn deduplicated(&self) -> u64 {
        self.standalone_size
            .saturating_sub(self.unique_variant_size)
    }
}

/// Canonical relative path of a blob inside a remote content store.
///
/// The path is derived from the digest, so the same content always resolves to
/// the same immutable location and a client never has to be told where to look
/// for a blob it was handed a digest for. Trust comes from a TUF-authenticated
/// release description that supplies the digests, not from this path.
pub fn remote_blob_path(digest: &Sha256Digest) -> String {
    format!("blobs/sha256/{}", digest.to_hex())
}

/// One variant's contribution to an artifact, with the digests it needs.
#[derive(Debug, Clone)]
pub struct VariantContentSet {
    pub id: String,
    pub digests: Vec<Sha256Digest>,
    pub logical_size: u64,
    pub unique_blob_count: u64,
}

/// Compute the savings account from the real content sets.
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

/// The target triples an index's variants cover, in variant order.
pub fn covered_targets(index: &ArtifactIndex) -> Vec<TargetTriple> {
    index
        .variants
        .iter()
        .map(|variant| variant.target.clone())
        .collect()
}

/// Requirements an index guarantees about every variant it carries.
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
