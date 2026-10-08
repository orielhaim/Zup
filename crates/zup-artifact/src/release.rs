use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_core::{App, Sha256Digest, TargetTriple};
use zup_signing::{FinalizedArtifact, Measured as FileMeasured, SigningEvidence, SigningSubject};

use crate::compat::LauncherSubsystem;
use crate::index::{ArtifactIndex, ArtifactKind, ArtifactMode, ArtifactPin};
use crate::platform::Platform;
use crate::variant::VariantDescriptor;

pub const RELEASE_SCHEMA: u32 = 1;

pub const RELEASE_MANIFEST_NAME: &str = "zup-release.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseVariant {
    pub id: String,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: zup_core::Frontend,
    pub logical_size: u64,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
    pub artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ReleaseRuntime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRuntime {
    pub digest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<SigningEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleTarget {
    pub id: String,
    pub version: semver::Version,
    pub target: TargetTriple,
    pub platform: Platform,
    pub frontend: zup_core::Frontend,
    pub subsystem: LauncherSubsystem,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    pub pin: ArtifactPin,
    pub path: String,
    pub built: BuildArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized: Option<FinalizedArtifact>,
    pub stored_size: u64,
    pub content_size: u64,
    pub unique_blob_count: u64,
    pub standalone_size: u64,
    pub shared_size: u64,
    pub variants: Vec<String>,
    pub subsystem: LauncherSubsystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildArtifact {
    pub digest: Sha256Digest,
    pub size: u64,
}

pub type SigningPlan = zup_signing::SigningPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub digest: Sha256Digest,
    pub size: u64,
    pub stored_size: u64,
    pub content_size: u64,
    pub unique_blob_count: u64,
    pub standalone_size: u64,
    pub shared_size: u64,
}

impl Measured {
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

    pub fn build_identity(&self) -> BuildArtifact {
        BuildArtifact {
            digest: self.digest,
            size: self.size,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub application: App,
    pub root: String,
    pub variants: Vec<ReleaseVariant>,
    pub artifacts: Vec<ReleaseArtifact>,
}

impl ReleaseManifest {
    pub fn new(application: &App) -> Self {
        Self {
            schema: RELEASE_SCHEMA,
            application: application.clone(),
            root: ".".to_owned(),
            variants: Vec::new(),
            artifacts: Vec::new(),
        }
    }

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

    pub fn add_composed(
        &mut self,
        artifact: &ReleaseArtifact,
        path: &std::path::Path,
    ) -> Result<(), ReleaseError> {
        require_relative(&artifact.path)?;
        let (size, digest) = measure(path)?;
        let mut recorded = artifact.clone();
        recorded.built = BuildArtifact { digest, size };
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

    pub fn bytes(&self) -> u64 {
        self.artifacts.iter().fold(0u64, |sum, artifact| {
            sum.saturating_add(self.published_size(artifact))
        })
    }

    pub fn published_size(&self, artifact: &ReleaseArtifact) -> u64 {
        artifact
            .finalized
            .as_ref()
            .map(FinalizedArtifact::size)
            .unwrap_or(artifact.built.size)
    }

    pub fn published_digest(&self, artifact: &ReleaseArtifact) -> Sha256Digest {
        artifact
            .finalized
            .as_ref()
            .map(|finalized| *finalized.digest())
            .unwrap_or(artifact.built.digest)
    }

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

    pub fn is_finalized(&self) -> bool {
        !self.artifacts.is_empty() && self.unfinalized().is_empty()
    }

    pub fn unfinalized(&self) -> Vec<&str> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.finalized.is_none())
            .map(|artifact| artifact.id.as_str())
            .collect()
    }

    /// Never names an unfinalized artifact: "unsigned" is a statement about
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

    pub fn is_signed(&self) -> bool {
        !self.artifacts.is_empty()
            && self.artifacts.iter().all(|artifact| {
                artifact
                    .finalized
                    .as_ref()
                    .is_some_and(FinalizedArtifact::is_signed)
            })
    }

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

    pub fn encode(&self) -> Result<Vec<u8>, ReleaseError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ReleaseError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        if manifest.schema != RELEASE_SCHEMA || manifest.root != "." {
            return Err(ReleaseError::Invalid);
        }
        Ok(manifest)
    }

    pub fn artifacts_by_id(&self) -> BTreeMap<&str, &ReleaseArtifact> {
        self.artifacts
            .iter()
            .map(|artifact| (artifact.id.as_str(), artifact))
            .collect()
    }
}

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

    fn claimed(path: &std::path::Path) -> FileMeasured {
        FileMeasured::of(path).expect("measure")
    }

    fn signed_evidence() -> Vec<SigningEvidence> {
        vec![
            SigningEvidence::new(zup_signing::EvidenceFact::SignatureCoversBytes, "sha256"),
            SigningEvidence::new(zup_signing::EvidenceFact::PlatformTrustAccepted, "windows"),
            SigningEvidence::new(zup_signing::EvidenceFact::Publisher, "CN=Acme"),
            SigningEvidence::new(zup_signing::EvidenceFact::Certificate, "0011"),
            SigningEvidence::new(zup_signing::EvidenceFact::Timestamp, "rfc3161"),
        ]
    }

    #[test]
    fn a_signed_artifact_publishes_the_signed_identity_not_the_built_one() {
        let (root, mut manifest) = release_with(b"unsigned", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        assert_eq!(manifest.artifacts[0].built.size, 8);
        assert!(manifest.artifacts[0].finalized.is_none());
        assert!(!manifest.is_finalized());

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

    #[test]
    fn a_file_that_changed_after_it_was_verified_is_refused() {
        let (root, mut manifest) = release_with(b"eight!!!", "dist/Acme-Setup.exe");
        let path = root.path().join("dist").join("Acme-Setup.exe");
        let verified = claimed(&path);

        std::fs::write(&path, b"eight!!!!").unwrap();
        assert!(matches!(
            manifest.finalize(root.path(), "windows-x64", &verified, signed_evidence()),
            Err(ReleaseError::NotFinalized { .. })
        ));
        assert!(!manifest.is_finalized());

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

        manifest
            .add_composed(&incoming, &path)
            .expect("a merge of unchanged bytes");
        assert!(manifest.is_finalized());

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

    #[test]
    fn a_parsed_description_requires_the_current_schema() {
        let mut manifest = ReleaseManifest::new(&app());
        manifest.schema = 2;
        assert!(matches!(
            ReleaseManifest::parse(&manifest.encode().unwrap()),
            Err(ReleaseError::Invalid)
        ));
    }

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
