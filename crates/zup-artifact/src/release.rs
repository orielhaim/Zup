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
// `zup_signing`'s `Measured` is a file measurement; the `Measured` in this
// module is what composition *accounts for*. They are different questions and
// the alias keeps them from reading as one.
use zup_signing::{FinalizedArtifact, Measured as FileMeasured, SigningEvidence, SigningSubject};

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
    /// The native runtime this variant's installer and maintenance executable are.
    ///
    /// Recorded because that runtime is a separate executable on a user's machine:
    /// `stage_variant` writes it out of the artifact and the maintenance
    /// executable, the elevated worker, and the uninstall runner are all that
    /// file. The outer artifact's signature covers these bytes as resource data
    /// and does not transfer to the extracted file, so a release that does not
    /// say which runtime it embeds cannot prove the executable that will actually
    /// run is one anybody signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ReleaseRuntime>,
}

/// The native runtime one variant executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRuntime {
    /// Digest of the runtime bytes, as they are embedded.
    pub digest: Sha256Digest,
    /// What is known about the signature over the runtime *file*, once one has
    /// been verified.
    ///
    /// Empty on a development build, and on any build whose runtime was never
    /// signed. Its presence is the difference between "an installer containing an
    /// unsigned executable" and "an installer containing a signed one", and it is
    /// the fact a downloader is entitled to rely on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<SigningEvidence>,
}

/// What one per-target installer reports.
///
/// A separate shape from [`ReleaseArtifact`] because the shape of the thing
/// differs: a per-target build has one variant, no dispatcher, and no shared
/// store, so naming those facts explicitly is cheaper than describing them with
/// a graph that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleTarget {
    /// The target profile, used as both the variant id and the artifact id.
    pub id: String,
    /// The application version this installer is pinned to.
    pub version: semver::Version,
    /// The target triple.
    pub target: TargetTriple,
    /// The platform the variant serves.
    pub platform: Platform,
    /// The presentation the runtime presents.
    pub frontend: zup_core::Frontend,
    /// The launcher experience the runtime's PE subsystem implies.
    pub subsystem: LauncherSubsystem,
    /// Files the installer's plan installs.
    pub file_count: u64,
    /// Prerequisite installers the plan declares.
    pub prerequisite_count: u64,
    /// Plugin components the plan carries.
    pub plugin_count: u64,
}

/// One user-facing file in the release.
///
/// The digest and size live in two places on purpose. `built` is what
/// composition produced; `finalized` is what will be published. Signing changes
/// PE bytes - it appends a certificate table and rewrites the checksum - so for
/// every signed artifact the two differ, and a manifest that published the
/// `built` pair under `finalized`'s name would describe bytes nobody can obtain.
///
/// `finalized` being absent is therefore a *fact about the release*, not a
/// missing field: it means the release has not been finalized, and every consumer
/// that would publish or download is expected to refuse it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    pub pin: ArtifactPin,
    /// Path relative to the release root, with `/` separators.
    pub path: String,
    /// The identity composition produced, before any signature.
    pub built: BuildArtifact,
    /// The identity that will be published, after signing.
    ///
    /// `None` until [`ReleaseManifest::finalize`] has measured the signed bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized: Option<FinalizedArtifact>,
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
}

/// What a file measured when it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildArtifact {
    /// Digest of the whole output file, so a build can prove which bytes it
    /// composed.
    pub digest: Sha256Digest,
    /// Bytes the file on disk occupies, dispatcher or runtime template included.
    pub size: u64,
}

/// The plan a build writes beside its release description.
///
/// The release layer re-exports the plan because the two documents are produced
/// together and read together; the plan's *meaning* is `zup-signing`'s, and
/// `zup-signing` does not know that a release description exists.
pub type SigningPlan = zup_signing::SigningPlan;

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

    /// The pre-sign identity of what this measurement describes.
    pub fn build_identity(&self) -> BuildArtifact {
        BuildArtifact {
            digest: self.digest,
            size: self.size,
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
                if entry.runtime.is_none() {
                    entry.runtime = variant.runtime.clone();
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
    /// digest can be wrong before the release is signed and published - and a
    /// release whose manifest disagrees with its files is not verifiable at all.
    ///
    /// A merged record keeps any finalization the incoming description carried
    /// only if the re-measured bytes agree with it. That comparison is the whole
    /// reason this re-measures: an artifact signed between the two jobs has
    /// different bytes than the merge source described, and adopting its
    /// finalization would publish a digest for bytes that are not there.
    pub fn add_composed(
        &mut self,
        artifact: &ReleaseArtifact,
        path: &std::path::Path,
    ) -> Result<(), ReleaseError> {
        require_relative(&artifact.path)?;
        let (size, digest) = measure(path)?;
        let mut recorded = artifact.clone();
        recorded.built = BuildArtifact { digest, size };
        // A merged record keeps an incoming finalization only if the re-measured
        // bytes still are what it says. An artifact signed between the two jobs
        // has different bytes than the merge source described, and adopting its
        // finalization would publish a digest for bytes that are not there.
        if recorded
            .finalized
            .as_ref()
            .is_some_and(|finalized| finalized.size() != size || finalized.digest() != &digest)
        {
            recorded.finalized = None;
        }
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
    ///
    /// The **final** size, because this is the number a host limit is checked
    /// against and the number a person is quoted. A pre-sign size is a smaller
    /// number for a signed artifact, so preflighting on it would accept a release
    /// the host then refuses mid-upload.
    pub fn bytes(&self) -> u64 {
        self.artifacts.iter().fold(0u64, |sum, artifact| {
            sum.saturating_add(self.published_size(artifact))
        })
    }

    /// The size this artifact will be published at, preferring the finalized
    /// measurement and falling back to the built one for an unfinalized release.
    pub fn published_size(&self, artifact: &ReleaseArtifact) -> u64 {
        artifact
            .finalized
            .as_ref()
            .map(FinalizedArtifact::size)
            .unwrap_or(artifact.built.size)
    }

    /// The digest this artifact will be published at, on the same terms as
    /// [`ReleaseManifest::published_size`].
    pub fn published_digest(&self, artifact: &ReleaseArtifact) -> Sha256Digest {
        artifact
            .finalized
            .as_ref()
            .map(|finalized| *finalized.digest())
            .unwrap_or(artifact.built.digest)
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
                    // An artifact that embeds a runtime names it, and the first
                    // one to do so wins: two artifacts carrying the same variant
                    // embed the same runtime bytes or one of them is wrong, and
                    // which one is a question the composition graph already
                    // answered.
                    if entry.runtime.is_none()
                        && let Some(runtime) = &variant.runtime
                    {
                        entry.runtime = Some(ReleaseRuntime {
                            digest: runtime.digest,
                            evidence: Vec::new(),
                        });
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
                    runtime: variant.runtime.as_ref().map(|runtime| ReleaseRuntime {
                        digest: runtime.digest,
                        evidence: Vec::new(),
                    }),
                }),
            }
        }
        self.artifacts.push(ReleaseArtifact {
            id: index.artifact.id.clone(),
            kind: index.artifact.kind,
            mode: index.artifact.mode,
            pin: index.artifact.pin.clone(),
            path: relative,
            built: measured.build_identity(),
            finalized: None,
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
        });
        self.variants.sort_by_key(|entry| entry.id.clone());
        self.artifacts.sort_by_key(|entry| entry.id.clone());
        Ok(())
    }

    /// Record a per-target installer.
    ///
    /// A single-target build has no graph: the artifact *is* the native runtime
    /// with its package embedded, there is no dispatcher, and the store is not
    /// shared with anything. Describing it by synthesizing an `ArtifactIndex`
    /// would put a composed graph into the model for something that has none, and
    /// would give the release a blob table describing zero bytes.
    pub fn add_single_target(
        &mut self,
        target: &SingleTarget,
        path: &str,
        measured: Measured,
    ) -> Result<(), ReleaseError> {
        let relative = require_relative(path)?;
        let id = target.id.clone();
        let variant = ReleaseVariant {
            id: id.clone(),
            target: target.target.clone(),
            platform: target.platform.clone(),
            frontend: target.frontend,
            logical_size: measured.standalone_size,
            file_count: target.file_count,
            prerequisite_count: target.prerequisite_count,
            plugin_count: target.plugin_count,
            artifacts: vec![id.clone()],
            // A per-target installer *is* its own runtime: the file a person runs
            // is the file that becomes the maintenance executable, the elevated
            // worker, and the uninstall runner. So its digest is recorded here and
            // the same signature covers both roles - there is no second executable
            // to carry separately.
            runtime: Some(ReleaseRuntime {
                digest: measured.digest,
                evidence: Vec::new(),
            }),
        };
        self.add_variant(&variant)?;
        self.artifacts.push(ReleaseArtifact {
            id,
            kind: ArtifactKind::Single,
            mode: ArtifactMode::Offline,
            pin: ArtifactPin::Pinned {
                version: target.version.clone(),
            },
            path: relative,
            built: measured.build_identity(),
            finalized: None,
            stored_size: measured.stored_size,
            content_size: measured.content_size,
            unique_blob_count: measured.unique_blob_count,
            standalone_size: measured.standalone_size,
            shared_size: measured.shared_size,
            variants: vec![target.id.clone()],
            subsystem: target.subsystem,
        });
        self.variants.sort_by(|left, right| left.id.cmp(&right.id));
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
            runtime: variant.runtime.as_ref().map(|runtime| ReleaseRuntime {
                digest: runtime.digest,
                evidence: Vec::new(),
            }),
        });
        self.variants.sort_by(|left, right| left.id.cmp(&right.id));
    }

    /// Whether every artifact in this description has been finalized.
    ///
    /// A publisher asks this before it writes anything. A release whose manifest
    /// is not finalized describes pre-sign bytes, and uploading it would put a
    /// manifest on the internet that no download can ever satisfy.
    pub fn is_finalized(&self) -> bool {
        !self.artifacts.is_empty() && self.unfinalized().is_empty()
    }

    /// The artifacts that still need a signature, by id.
    pub fn unfinalized(&self) -> Vec<&str> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.finalized.is_none())
            .map(|artifact| artifact.id.as_str())
            .collect()
    }

    /// The artifacts a consumer must be told are unsigned.
    ///
    /// Never names an unfinalized artifact: "unsigned" is a statement about
    /// published bytes, and an artifact with no published bytes has no such
    /// statement to make.
    pub fn unsigned(&self) -> Vec<&str> {
        self.artifacts
            .iter()
            .filter(|artifact| {
                artifact
                    .finalized
                    .as_ref()
                    .is_some_and(|finalized| !finalized.is_signed())
            })
            .map(|artifact| artifact.id.as_str())
            .collect()
    }

    /// Whether every artifact carries a platform signature.
    pub fn is_signed(&self) -> bool {
        !self.artifacts.is_empty()
            && self.artifacts.iter().all(|artifact| {
                artifact
                    .finalized
                    .as_ref()
                    .is_some_and(FinalizedArtifact::is_signed)
            })
    }

    /// Record that `id` is finalized, and measure the bytes that will be
    /// published.
    ///
    /// `claimed` is the measurement the caller took at the moment it verified
    /// the signature, and this method measures the file again. `root` is the
    /// release root the artifact's own `path` is relative to - the caller is
    /// handed it rather than having it guessed from a file path, because
    /// `dist/Acme-Setup.exe` and the release root are different lengths and
    /// inferring one from the other resolves to a directory that does not
    /// exist.
    ///
    /// `evidence` is the *result* of verifying the signature, not a claim that
    /// one exists. This crate cannot evaluate an Authenticode signature - it is
    /// portable, and Authenticode is not - so the platform adapter verifies it
    /// and hands over what it found. What this method adds is the part that is
    /// portable and that a signature check alone does not give: the published
    /// identity is a measurement rather than a transcription of what the signer
    /// said it did. That also closes the window in which a file could change
    /// between the two.
    pub fn finalize(
        &mut self,
        root: &std::path::Path,
        id: &str,
        claimed: &FileMeasured,
        evidence: Vec<SigningEvidence>,
    ) -> Result<(), ReleaseError> {
        let artifact = self
            .artifacts
            .iter()
            .find(|artifact| artifact.id == id)
            .ok_or_else(|| ReleaseError::UnknownArtifact { id: id.to_owned() })?;
        let subject = SigningSubject {
            path: artifact.path.clone(),
            digest: artifact.built.digest,
            size: artifact.built.size,
            variants: artifact.variants.clone(),
        };
        let finalized =
            zup_signing::finalize(&subject, root, claimed, evidence).map_err(|error| {
                ReleaseError::NotFinalized {
                    id: id.to_owned(),
                    reason: error.to_string(),
                }
            })?;
        self.artifacts
            .iter_mut()
            .find(|artifact| artifact.id == id)
            .expect("found above")
            .finalized = Some(finalized);
        Ok(())
    }

    /// Record what is known about the signature over the runtime `variant`
    /// executes.
    ///
    /// Separate from [`ReleaseManifest::finalize`] because the two are different
    /// files. Finalizing says "these are the bytes we publish"; this says "the
    /// executable a user ends up running is one somebody signed". A composed
    /// artifact can be finalized with a perfectly good outer signature while its
    /// extracted runtime is unsigned, and a downloader is entitled to know which
    /// of the two happened.
    pub fn note_runtime_evidence(
        &mut self,
        variant: &str,
        evidence: Vec<SigningEvidence>,
    ) -> Result<(), ReleaseError> {
        let entry = self
            .variants
            .iter_mut()
            .find(|entry| entry.id == variant)
            .ok_or_else(|| ReleaseError::UnknownVariant {
                id: variant.to_owned(),
            })?;
        let runtime = entry
            .runtime
            .as_mut()
            .ok_or_else(|| ReleaseError::UnknownVariant {
                id: variant.to_owned(),
            })?;
        runtime.evidence = evidence;
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
    #[error("`{id}` could not be finalized: {reason}")]
    NotFinalized { id: String, reason: String },
    #[error("the release has not been finalized; these artifacts carry no final identity: {}", ids.join(", "))]
    Unfinalized { ids: Vec<String> },
}

/// Measure a file's size and digest, refusing anything unreadable.
fn measure(path: &std::path::Path) -> Result<(u64, Sha256Digest), ReleaseError> {
    let file = std::fs::File::open(path).map_err(|_| ReleaseError::Invalid)?;
    let (size, digest) =
        zup_core::hash_reader(std::io::BufReader::new(file)).map_err(|_| ReleaseError::Invalid)?;
    Ok((size, digest))
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

    fn single_target() -> SingleTarget {
        SingleTarget {
            id: "windows-x64".to_owned(),
            version: Version::parse("1.4.0").unwrap(),
            target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            platform: Platform::from_triple(
                &TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            ),
            frontend: zup_core::Frontend::Gui,
            subsystem: LauncherSubsystem::Gui,
            file_count: 12,
            prerequisite_count: 0,
            plugin_count: 0,
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

    /// A release, one installer, and the file on disk beside it.
    fn release_with(bytes: &[u8], path: &str) -> (tempfile::TempDir, ReleaseManifest) {
        let root = tempfile::TempDir::new().unwrap();
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().expect("a parent")).unwrap();
        std::fs::write(&file, bytes).unwrap();
        let mut manifest = ReleaseManifest::new(&app());
        manifest
            .add_single_target(
                &single_target(),
                path,
                Measured::single(
                    zup_core::Sha256Digest::from_bytes([0x11; 32]),
                    bytes.len() as u64,
                    bytes.len() as u64,
                ),
            )
            .unwrap();
        (root, manifest)
    }

    /// The measurement a caller takes the moment it verified a signature, and
    /// hands over for the release to check against.
    fn claimed(path: &std::path::Path) -> FileMeasured {
        FileMeasured::of(path).expect("measure")
    }

    /// The evidence a platform adapter hands over once it has verified a
    /// signature. Every entry is a public identity or a fact about one.
    fn signed_evidence() -> Vec<SigningEvidence> {
        vec![
            SigningEvidence::new(zup_signing::EvidenceFact::SignatureCoversBytes, "sha256"),
            SigningEvidence::new(zup_signing::EvidenceFact::PlatformTrustAccepted, "windows"),
            SigningEvidence::new(zup_signing::EvidenceFact::Publisher, "CN=Acme"),
            SigningEvidence::new(zup_signing::EvidenceFact::Certificate, "0011"),
            SigningEvidence::new(zup_signing::EvidenceFact::Timestamp, "rfc3161"),
        ]
    }

    /// The whole point of the two identities: composition measures bytes, signing
    /// changes them, and the release publishes what was signed.
    #[test]
    fn a_signed_artifact_publishes_the_signed_identity_not_the_built_one() {
        let (root, mut manifest) = release_with(b"unsigned", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        assert_eq!(manifest.artifacts[0].built.size, 8);
        assert!(manifest.artifacts[0].finalized.is_none());
        assert!(!manifest.is_finalized());

        // The signer appends a certificate table: the file grows, and its digest
        // changes. Both are re-measured, and both differ from `built`.
        std::fs::write(&path, b"unsigned plus a certificate table").unwrap();
        let measured = claimed(&path);
        assert_ne!(measured.size, 8);
        manifest
            .finalize(root.path(), "windows-x64", &measured, signed_evidence())
            .unwrap();

        assert!(manifest.is_finalized());
        assert!(manifest.is_signed());
        assert!(manifest.unsigned().is_empty());
        let artifact = &manifest.artifacts[0];
        assert_eq!(artifact.finalized.as_ref().unwrap().size(), measured.size);
        assert_eq!(
            artifact.finalized.as_ref().unwrap().digest(),
            &measured.digest
        );
        assert_eq!(manifest.bytes(), measured.size);
        assert_eq!(manifest.unfinalized(), Vec::<&str>::new());
    }

    /// The published identity is measured, not transcribed, and the two
    /// measurements must agree. This is the window in which a file could change
    /// after its signature was verified and before the release recorded a digest
    /// for it.
    #[test]
    fn a_file_that_changed_after_it_was_verified_is_refused() {
        let (root, mut manifest) = release_with(b"eight!!!", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        let verified = claimed(&path);

        // The file changes after the signature was verified. The identity the
        // caller holds now describes bytes that are not on disk, so nothing is
        // recorded and a publisher would still refuse the release.
        std::fs::write(&path, b"eight!!!!").unwrap();
        assert!(matches!(
            manifest.finalize(root.path(), "windows-x64", &verified, signed_evidence()),
            Err(ReleaseError::NotFinalized { .. })
        ));
        assert!(!manifest.is_finalized());

        // Measured against the bytes that are actually there, it finalizes, and
        // the published size is that measurement.
        let measured = claimed(&path);
        manifest
            .finalize(root.path(), "windows-x64", &measured, signed_evidence())
            .unwrap();
        assert!(manifest.is_finalized());
        assert_eq!(
            manifest.published_size(&manifest.artifacts[0]),
            measured.size
        );
    }

    /// A merge between two jobs re-measures, and adopts a finalization only if
    /// the bytes are still the ones it describes. An artifact signed between the
    /// two jobs has different bytes, and publishing the source's digest would
    /// describe a file that is not there.
    #[test]
    fn a_merge_adopts_a_finalization_only_while_the_bytes_still_match() {
        let (root, mut manifest) = release_with(b"eight bytes", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        manifest
            .finalize(
                root.path(),
                "windows-x64",
                &claimed(&path),
                signed_evidence(),
            )
            .unwrap();
        let incoming = manifest.artifacts[0].clone();

        // The same bytes: the finalization is still true of them.
        manifest
            .add_composed(&incoming, &path)
            .expect("a merge of unchanged bytes");
        assert!(manifest.is_finalized());

        // The file changed after the source recorded its identity.
        std::fs::write(&path, b"eight bytes plus a certificate table").unwrap();
        manifest
            .add_composed(&incoming, &path)
            .expect("a merge of changed bytes");
        assert!(
            manifest.artifacts[0].finalized.is_none(),
            "a finalization for bytes that are no longer there is dropped"
        );
        assert!(!manifest.is_finalized());
    }

    /// The runtime a variant executes is a different file from the artifact that
    /// embeds it, and a downloader is entitled to know whether that one is signed.
    /// The runtime a variant executes is a different file from the artifact that
    /// embeds it, and a downloader is entitled to know whether that one is signed.
    /// Composition knows the runtime's digest and nothing about a signature, so the
    /// evidence arrives later and is filed against the variant rather than the
    /// artifact.
    #[test]
    fn a_signed_runtime_is_recorded_separately_from_the_artifact() {
        let (_, mut manifest) = release_with(b"unsigned!", "dist/Acme-Setup.exe");
        let variant = &manifest.variants[0];
        assert!(variant.runtime.is_some());
        assert!(
            variant.runtime.as_ref().unwrap().evidence.is_empty(),
            "composition knows the runtime's digest and nothing about a signature"
        );
        manifest
            .note_runtime_evidence("windows-x64", signed_evidence())
            .unwrap();
        let evidence = &manifest.variants[0].runtime.as_ref().unwrap().evidence;
        assert_eq!(zup_signing::publisher(evidence), Some("CN=Acme"));
        assert!(matches!(
            manifest.note_runtime_evidence("nope", signed_evidence()),
            Err(ReleaseError::UnknownVariant { .. })
        ));
    }

    /// A description that parses must be one this build wrote, or a peer would
    /// publish bytes none of its fields describe.
    #[test]
    fn a_parsed_description_requires_the_current_schema() {
        let mut manifest = ReleaseManifest::new(&app());
        manifest.schema = 2;
        assert!(matches!(
            ReleaseManifest::parse(&manifest.encode().unwrap()),
            Err(ReleaseError::Invalid)
        ));
    }

    /// A description that parsed is a description every query can address: the
    /// ids are the keys, and the two finalization questions are a partition of
    /// the artifacts rather than overlapping claims. Every artifact is either
    /// finalized or named by `unfinalized`; every finalized one is either
    /// signed or named by `unsigned`.
    #[test]
    fn the_two_finalization_questions_partition_the_artifacts() {
        let (root, mut manifest) = release_with(b"unsigned!", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        manifest
            .finalize(
                root.path(),
                "windows-x64",
                &claimed(&path),
                signed_evidence(),
            )
            .unwrap();
        assert!(manifest.is_finalized());
        assert!(manifest.unfinalized().is_empty());
        assert!(manifest.unsigned().is_empty());
        assert!(manifest.is_signed());

        // The same questions asked of an artifact with no signature, where the
        // answers swap and the artifact is named instead.
        let (root, mut manifest) = release_with(b"unsigned!", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        manifest
            .finalize(root.path(), "windows-x64", &claimed(&path), Vec::new())
            .unwrap();
        let id = manifest.artifacts[0].id.as_str();
        assert!(manifest.is_finalized());
        assert!(manifest.unfinalized().is_empty());
        assert_eq!(manifest.unsigned(), vec![id]);
        assert!(!manifest.is_signed());
        assert_eq!(
            manifest.unsigned().is_empty(),
            manifest.is_signed(),
            "either every artifact is signed or the ones that are not are named"
        );

        assert!(
            manifest
                .unfinalized()
                .iter()
                .all(|unfinalized| !manifest.unsigned().contains(unfinalized)),
            "`unsigned` and `unfinalized` are different questions and never name the same artifact"
        );
    }
}
