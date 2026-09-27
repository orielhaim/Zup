//! The release description a build emits.
//!
//! This is the bridge to everything outside the build: CI matrices, signing
//! pipelines, CDN uploads, container registries, and the provenance and
//! software-bill-of-materials records that attach to a release later. It
//! describes what was produced, never where the build machine kept it: every
//! path is relative to the release root, so the document is identical on every
//! machine that produced the same bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_core::{App, Sha256Digest, TargetTriple};

use crate::compat::LauncherSubsystem;
use crate::index::{ArtifactIndex, ArtifactKind, ArtifactMode, ArtifactPin};
use crate::platform::Platform;
use crate::variant::VariantDescriptor;

/// Current release description schema.
pub const RELEASE_SCHEMA: u32 = 1;

/// The file name a build writes its release description to.
pub const RELEASE_MANIFEST_NAME: &str = "zup-release.json";

/// One variant in the release, and the artifacts that carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseVariant {
    pub id: String,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: zup_core::Frontend,
    /// Logical payload size, counting shared content once per variant.
    pub logical_size: u64,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
    /// Artifacts that include this variant.
    pub artifacts: Vec<String>,
}

/// One user-facing file in the release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    pub pin: ArtifactPin,
    /// Path relative to the release root, with `/` separators.
    pub path: String,
    pub digest: Sha256Digest,
    pub size: u64,
    /// Compressed bytes of the store the artifact carries.
    pub stored_size: u64,
    /// Uncompressed bytes of the store the artifact carries.
    pub content_size: u64,
    pub unique_blob_count: u64,
    /// Sum of the included variants' logical sizes.
    pub standalone_size: u64,
    /// Bytes more than one included variant needs.
    pub shared_size: u64,
    pub variants: Vec<String>,
    pub subsystem: LauncherSubsystem,
    /// Trust status the build can know. Signing happens after composition, so
    /// a build reports what it can prove and leaves the rest to the pipeline
    /// that signs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureStatus>,
}

/// What is known about an artifact's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SignatureStatus {
    /// The build did not sign this artifact.
    Unsigned,
    /// The artifact was composed unsigned and is ready to be signed.
    ReadyToSign,
    /// A signature covers the composed bytes.
    Signed { subject: String },
}

/// What one artifact's output file measured.
///
/// The two shapes are not the same measurement, so they have two constructors.
/// A composed artifact has a store and accounts for what sharing saved; a
/// single-target installer has neither, and reporting it as though it did would
/// put zeros in a published document where a reader expects numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    /// Digest of the whole output file, so a publisher can verify what it shipped.
    pub digest: Sha256Digest,
    /// Bytes the file on disk occupies, dispatcher or runtime template included.
    pub size: u64,
    /// Bytes the content store holds, compressed.
    pub stored_size: u64,
    /// Bytes the store holds before compression.
    pub content_size: u64,
    /// Distinct blobs in the store. One file is one blob.
    pub unique_blob_count: u64,
    /// What these variants would have cost as separate installers.
    pub standalone_size: u64,
    /// The part of `standalone_size` that composition stored once.
    pub shared_size: u64,
}

impl Measured {
    /// A composed artifact: one store, shared content stored once.
    #[allow(clippy::too_many_arguments)]
    pub fn composed(
        digest: Sha256Digest,
        size: u64,
        stored_size: u64,
        content_size: u64,
        unique_blob_count: u64,
        standalone_size: u64,
        shared_size: u64,
    ) -> Self {
        Self {
            digest,
            size,
            stored_size,
            content_size,
            unique_blob_count,
            standalone_size,
            shared_size,
        }
    }

    /// A single-target installer: its content is the file, and nothing is shared
    /// because there is only one variant.
    pub fn single(digest: Sha256Digest, size: u64, content_size: u64) -> Self {
        Self {
            digest,
            size,
            stored_size: content_size,
            content_size,
            unique_blob_count: 1,
            standalone_size: size,
            shared_size: 0,
        }
    }
}

/// The machine-readable description of one build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub application: App,
    /// Release root every `path` is relative to, always `.`.
    pub root: String,
    pub variants: Vec<ReleaseVariant>,
    pub artifacts: Vec<ReleaseArtifact>,
}

impl ReleaseManifest {
    /// Start a description for `application`.
    pub fn new(application: &App) -> Self {
        Self {
            schema: RELEASE_SCHEMA,
            application: application.clone(),
            root: ".".to_owned(),
            variants: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// Record a variant without an artifact.
    ///
    /// For a description assembled from per-target builds rather than composed
    /// in one place: the variant's own numbers are already known, and the
    /// artifact that will carry it may be recorded by a later merge.
    pub fn add_variant(&mut self, variant: &ReleaseVariant) -> Result<(), ReleaseError> {
        match self
            .variants
            .iter_mut()
            .find(|entry| entry.id == variant.id)
        {
            Some(entry) => {
                if entry.target != variant.target {
                    return Err(ReleaseError::Invalid);
                }
                for artifact in &variant.artifacts {
                    if !entry.artifacts.contains(artifact) {
                        entry.artifacts.push(artifact.clone());
                    }
                }
            }
            None => self.variants.push(variant.clone()),
        }
        Ok(())
    }

    /// Record a file another job produced, re-measuring it here.
    ///
    /// The digest is recomputed from the bytes on disk rather than copied from
    /// the description that claimed them, because the merge is the last place a
    /// digest can be wrong before the release is signed and published — and a
    /// release whose manifest disagrees with its files is not verifiable at all.
    pub fn add_composed(
        &mut self,
        artifact: &ReleaseArtifact,
        path: &std::path::Path,
    ) -> Result<(), ReleaseError> {
        require_relative(&artifact.path)?;
        let file = std::fs::File::open(path).map_err(|_| ReleaseError::Invalid)?;
        let (_, digest) = zup_core::hash_reader(std::io::BufReader::new(file))
            .map_err(|_| ReleaseError::Invalid)?;
        let mut recorded = artifact.clone();
        recorded.digest = digest;
        match self
            .artifacts
            .iter_mut()
            .find(|entry| entry.id == artifact.id)
        {
            Some(entry) => *entry = recorded,
            None => self.artifacts.push(recorded),
        }
        Ok(())
    }

    /// The total bytes every artifact in this description takes.
    pub fn bytes(&self) -> u64 {
        self.artifacts
            .iter()
            .fold(0u64, |sum, artifact| sum.saturating_add(artifact.size))
    }

    /// Record a composed artifact and what its output file measured.
    ///
    /// The measurements are a group rather than a list of arguments because they
    /// are read together: `size` is the file, `stored_size` is what the store
    /// holds, `content_size` is that content before compression,
    /// `standalone_size` is what the same variants would have cost as separate
    /// installers, and `shared_size` is the difference composition accounted
    /// for. A caller that cannot state all of them is not describing a
    /// composed artifact.
    pub fn add_artifact(
        &mut self,
        index: &ArtifactIndex,
        path: &str,
        measured: Measured,
    ) -> Result<(), ReleaseError> {
        let relative = require_relative(path)?;
        for variant in &index.variants {
            match self
                .variants
                .iter_mut()
                .find(|entry| entry.id == variant.id)
            {
                Some(entry) => {
                    if !entry.artifacts.contains(&index.artifact.id) {
                        entry.artifacts.push(index.artifact.id.clone());
                    }
                }
                None => self.variants.push(ReleaseVariant {
                    id: variant.id.clone(),
                    target: variant.target.clone(),
                    platform: variant.platform.clone(),
                    frontend: variant.frontend,
                    logical_size: variant.logical_size,
                    file_count: variant.content.file_count,
                    prerequisite_count: variant.content.prerequisite_count,
                    plugin_count: variant.content.plugin_count,
                    artifacts: vec![index.artifact.id.clone()],
                }),
            }
        }
        self.artifacts.push(ReleaseArtifact {
            id: index.artifact.id.clone(),
            kind: index.artifact.kind,
            mode: index.artifact.mode,
            pin: index.artifact.pin.clone(),
            path: relative,
            digest: measured.digest,
            size: measured.size,
            stored_size: measured.stored_size,
            content_size: measured.content_size,
            unique_blob_count: measured.unique_blob_count,
            standalone_size: measured.standalone_size,
            shared_size: measured.shared_size,
            variants: index
                .variant_ids()
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
            subsystem: index.artifact.subsystem,
            signature: Some(SignatureStatus::ReadyToSign),
        });
        self.variants.sort_by_key(|entry| entry.id.clone());
        self.artifacts.sort_by_key(|entry| entry.id.clone());
        Ok(())
    }

    /// Record a variant that no artifact carries, which is what a per-target
    /// debug build produces.
    pub fn add_standalone_variant(&mut self, variant: &VariantDescriptor) {
        self.variants.push(ReleaseVariant {
            id: variant.id.clone(),
            target: variant.target.clone(),
            platform: variant.platform.clone(),
            frontend: variant.frontend,
            logical_size: variant.logical_size,
            file_count: variant.content.file_count,
            prerequisite_count: variant.content.prerequisite_count,
            plugin_count: variant.content.plugin_count,
            artifacts: Vec::new(),
        });
        self.variants.sort_by(|left, right| left.id.cmp(&right.id));
    }

    /// Mark one artifact as signed by `subject`.
    pub fn mark_signed(
        &mut self,
        id: &str,
        subject: impl Into<String>,
    ) -> Result<(), ReleaseError> {
        let artifact = self
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.id == id)
            .ok_or_else(|| ReleaseError::UnknownArtifact { id: id.to_owned() })?;
        artifact.signature = Some(SignatureStatus::Signed {
            subject: subject.into(),
        });
        Ok(())
    }

    /// Serialize canonically.
    pub fn encode(&self) -> Result<Vec<u8>, ReleaseError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse a description, rejecting an unrecognized schema before
    /// deserializing the rest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ReleaseError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        if manifest.schema != RELEASE_SCHEMA || manifest.root != "." {
            return Err(ReleaseError::Invalid);
        }
        Ok(manifest)
    }

    /// The artifacts a release publishes, keyed by id.
    pub fn artifacts_by_id(&self) -> BTreeMap<&str, &ReleaseArtifact> {
        self.artifacts
            .iter()
            .map(|artifact| (artifact.id.as_str(), artifact))
            .collect()
    }
}

/// Failures produced while building a release description.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("release description JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release path `{0}` is not relative to the release root")]
    AbsolutePath(String),
    #[error("release description names unknown variant `{id}`")]
    UnknownVariant { id: String },
    #[error("release description names unknown artifact `{id}`")]
    UnknownArtifact { id: String },
    #[error("release description is malformed")]
    Invalid,
}

fn require_relative(path: &str) -> Result<String, ReleaseError> {
    let trimmed = path.replace('\\', "/");
    let bytes = trimmed.as_bytes();
    let absolute = trimmed.starts_with('/')
        || bytes.get(1) == Some(&b':')
        || trimmed.contains(':')
        || trimmed.split('/').any(|segment| segment == "..");
    if absolute || trimmed.is_empty() {
        return Err(ReleaseError::AbsolutePath(path.to_owned()));
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use zup_core::{AppId, NonEmptyString};

    fn app() -> App {
        App {
            id: AppId::new("com.acme.desktop").unwrap(),
            name: NonEmptyString::new("Acme").unwrap(),
            version: Version::parse("1.4.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        }
    }

    #[test]
    fn a_build_machine_path_is_refused() {
        assert!(require_relative("dist/Acme-Setup.exe").is_ok());
        assert!(require_relative("dist\\Acme-Setup.exe").is_ok());
        assert!(require_relative(r"C:\out\Acme.exe").is_err());
        assert!(require_relative("/out/Acme.exe").is_err());
        assert!(require_relative("../Acme.exe").is_err());
        assert!(require_relative("").is_err());
    }

    #[test]
    fn encoding_is_deterministic() {
        let manifest = ReleaseManifest::new(&app());
        assert_eq!(manifest.encode().unwrap(), manifest.encode().unwrap());
        let parsed = ReleaseManifest::parse(&manifest.encode().unwrap()).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn a_parsed_description_requires_the_current_schema() {
        let mut manifest = ReleaseManifest::new(&app());
        manifest.schema = 2;
        assert!(matches!(
            ReleaseManifest::parse(&manifest.encode().unwrap()),
            Err(ReleaseError::Invalid)
        ));
    }
}
