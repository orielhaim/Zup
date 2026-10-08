use std::path::{Path, PathBuf};

use zup_acquire::{
    CachePolicy, ContentCache, ContentCatalog, ReleaseDescriptor, RuntimeHandoff, Verify,
};
use zup_binary::{Executable, ProgramKind};
use zup_bundle::{AcquiredPayloadSource, Package, PayloadError};
use zup_core::{Frontend, Sha256Digest};

use crate::bundle_packager::EmbeddedBundle;
use crate::durable::write_durable;

pub struct AcquiredRelease {
    pub release: ReleaseDescriptor,

    pub variant: String,

    pub manifest: Vec<u8>,

    pub catalog: ContentCatalog,

    pub cache: ContentCache,

    pub handoff_path: PathBuf,

    pub handoff_digest: Sha256Digest,
}

#[derive(Debug, thiserror::Error)]
pub enum HandoffRejection {
    #[error("the handoff document could not be read: {0}")]
    Unreadable(String),
    #[error("the handoff document is not a handoff: {0}")]
    Malformed(#[from] zup_acquire::HandoffError),
    #[error(
        "this executable is {found}, but the authenticated release names {expected} for the selected variant"
    )]
    NotTheNamedRuntime {
        expected: Sha256Digest,
        found: Sha256Digest,
    },
    #[error("the authenticated release {release} does not carry variant `{variant}`")]
    UnknownVariant { release: String, variant: String },
    #[error(
        "the handoff names variant `{handoff}` but the release's variant descriptor is `{release}`"
    )]
    VariantDescriptorMismatch {
        handoff: Sha256Digest,
        release: Sha256Digest,
    },
    #[error("the handoff names release {handoff} but the cached document is {document}")]
    ReleaseMismatch {
        handoff: Sha256Digest,
        document: Sha256Digest,
    },
    #[error("the handoff names catalog {handoff} but the release authenticates {release}")]
    CatalogMismatch {
        handoff: Sha256Digest,
        release: Sha256Digest,
    },
    #[error("the handoff targets {handoff} but this executable was built for {executable}")]
    TargetMismatch { handoff: String, executable: String },
    #[error("the handoff presents the {handoff} frontend but this executable is {executable}")]
    FrontendMismatch { handoff: String, executable: String },
    #[error("the handoff is for {handoff} but this executable installs {executable}")]
    ApplicationMismatch { handoff: String, executable: String },
    #[error("the authenticated release does not carry a native runtime for `{0}`")]
    NoRuntime(String),
    #[error("this executable carries no package: {0}")]
    NoPackage(#[from] crate::bundle_packager::BundleError),
    #[error("acquired release: {0}")]
    Acquire(#[from] zup_acquire::CacheError),
    #[error("handoff I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("handoff write: {0}")]
    Durable(#[from] crate::durable::DurableError),
}

impl HandoffRejection {
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}

pub fn own_digest(executable: &Path) -> Result<Sha256Digest, std::io::Error> {
    let (_, digest) = zup_core::hash_reader(std::fs::File::open(executable)?)?;
    Ok(digest)
}

pub fn accept_handoff(
    handoff_path: &Path,
    expected: Option<Sha256Digest>,
) -> Result<RuntimeHandoff, HandoffRejection> {
    let bytes = std::fs::read(handoff_path)
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
    let handoff = RuntimeHandoff::parse(&bytes)?;
    if let Some(expected) = expected
        && handoff.digest() != expected
    {
        return Err(HandoffRejection::Unreadable(
            "the handoff document does not match the digest the launcher passed".into(),
        ));
    }
    Ok(handoff)
}

pub struct VerifiedHandoff {
    pub handoff: RuntimeHandoff,
    pub release: ReleaseDescriptor,
    pub catalog: ContentCatalog,
    pub manifest: Vec<u8>,
}

impl std::fmt::Debug for VerifiedHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedHandoff")
            .field("handoff", &self.handoff)
            .field("release", &self.release.release_digest)
            .field("catalog", &self.catalog.blobs.len())
            .field("manifest_bytes", &self.manifest.len())
            .finish()
    }
}

pub fn verify(
    executable: &Path,
    cache: &ContentCache,
    handoff: &RuntimeHandoff,
) -> Result<VerifiedHandoff, HandoffRejection> {
    let release = read_document(cache, handoff.document, "release descriptor")?;
    let descriptor = ReleaseDescriptor::parse(&release)
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
    if descriptor.release_digest != handoff.release {
        return Err(HandoffRejection::ReleaseMismatch {
            handoff: handoff.release,
            document: descriptor.release_digest,
        });
    }
    if descriptor.catalog.digest != handoff.catalog {
        return Err(HandoffRejection::CatalogMismatch {
            handoff: handoff.catalog,
            release: descriptor.catalog.digest,
        });
    }

    let variant = descriptor
        .variant(&handoff.variant)
        .ok_or_else(|| HandoffRejection::UnknownVariant {
            release: descriptor.version.clone(),
            variant: handoff.variant.clone(),
        })?
        .clone();
    if variant.manifest.digest != handoff.manifest {
        return Err(HandoffRejection::VariantDescriptorMismatch {
            handoff: handoff.manifest,
            release: variant.manifest.digest,
        });
    }
    let runtime = variant
        .runtime
        .as_ref()
        .ok_or_else(|| HandoffRejection::NoRuntime(variant.id.clone()))?;
    if runtime.digest != handoff.runtime {
        return Err(HandoffRejection::NotTheNamedRuntime {
            expected: runtime.digest,
            found: handoff.runtime,
        });
    }

    let found = own_digest(executable)?;
    if found != runtime.digest {
        return Err(HandoffRejection::NotTheNamedRuntime {
            expected: runtime.digest,
            found,
        });
    }

    if descriptor.app_id != handoff.app_id {
        return Err(HandoffRejection::ApplicationMismatch {
            handoff: handoff.app_id.to_string(),
            executable: descriptor.app_id.to_string(),
        });
    }
    if variant.target != handoff.target {
        return Err(HandoffRejection::TargetMismatch {
            handoff: handoff.target.to_string(),
            executable: variant.target.to_string(),
        });
    }
    if variant.frontend != handoff.frontend {
        return Err(HandoffRejection::FrontendMismatch {
            handoff: handoff.frontend.clone(),
            executable: variant.frontend.clone(),
        });
    }

    let built_for = zup_binary::Executable::read(executable)
        .map_err(|error| HandoffRejection::Unreadable(format!("this executable: {error}")))?;
    built_for
        .refuse_target(&handoff.target)
        .map_err(|error| HandoffRejection::TargetMismatch {
            handoff: handoff.target.to_string(),
            executable: error.to_string(),
        })?;

    let manifest = read_document(cache, variant.manifest.digest, "variant manifest")?;
    let catalog = read_document(cache, descriptor.catalog.digest, "content catalog")?;
    let catalog = ContentCatalog::parse(&catalog)
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;

    Ok(VerifiedHandoff {
        handoff: handoff.clone(),
        release: descriptor,
        catalog,
        manifest,
    })
}

fn read_document(
    cache: &ContentCache,
    digest: Sha256Digest,
    what: &str,
) -> Result<Vec<u8>, HandoffRejection> {
    let mut path = cache.root().to_path_buf();
    for segment in zup_acquire::blob_path(&digest).to_string().split('/') {
        path.push(segment);
    }
    let size = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if size == 0 {
        return Err(HandoffRejection::Unreadable(format!(
            "the {what} {digest} is not in the verified cache"
        )));
    }
    let descriptor =
        zup_acquire::ContentDescriptor::stored(zup_acquire::ContentKind::Metadata, digest, size);
    let blob = cache.get(&descriptor, Verify::Full)?.ok_or_else(|| {
        HandoffRejection::Unreadable(format!("the {what} {digest} did not verify"))
    })?;
    blob.read_to_end()
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))
}

pub struct AcquiredBundle {
    pub package: Package,

    pub release: ReleaseDescriptor,
    pub variant: String,
    pub catalog: ContentCatalog,
}

impl std::fmt::Debug for AcquiredBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcquiredBundle")
            .field("package_blobs", &self.package.blob_count())
            .field("release", &self.release.release_digest)
            .field("variant", &self.variant)
            .field("catalog", &self.catalog.blobs.len())
            .finish()
    }
}

impl AcquiredBundle {
    pub fn open(
        executable: &Path,
        _cache: &ContentCache,
        verified: &VerifiedHandoff,
    ) -> Result<Self, HandoffRejection> {
        let embedded = EmbeddedBundle::open(executable)?;
        let package = embedded.package().clone();
        if !package.is_plan_only() {
            return Err(HandoffRejection::Unreadable(
                "this executable carries its own payload, so it is not a thin runtime".into(),
            ));
        }
        let manifest = zup_artifact::VariantManifest::parse(&verified.manifest)
            .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        let selected = verified
            .release
            .variant(&verified.handoff.variant)
            .ok_or_else(|| HandoffRejection::UnknownVariant {
                release: verified.release.version.clone(),
                variant: verified.handoff.variant.clone(),
            })?;

        if manifest.plan.installer.app.id != verified.release.app_id {
            return Err(HandoffRejection::ApplicationMismatch {
                handoff: verified.release.app_id.to_string(),
                executable: manifest.plan.installer.app.id.to_string(),
            });
        }
        if manifest.target != selected.target {
            return Err(HandoffRejection::TargetMismatch {
                handoff: selected.target.to_string(),
                executable: manifest.target.to_string(),
            });
        }
        if manifest.frontend != package.plan().installer.frontend {
            return Err(HandoffRejection::FrontendMismatch {
                handoff: manifest.frontend.to_string(),
                executable: package.plan().installer.frontend.to_string(),
            });
        }
        if manifest.plan != *package.plan() {
            return Err(HandoffRejection::Unreadable(
                "the authenticated variant manifest and this executable's embedded plan differ"
                    .into(),
            ));
        }
        Ok(Self {
            package,
            release: verified.release.clone(),
            variant: verified.handoff.variant.clone(),
            catalog: verified.catalog.clone(),
        })
    }

    pub fn target(&self) -> &zup_core::TargetTriple {
        &self.package.plan().installer.target
    }

    pub fn frontend(&self) -> zup_core::Frontend {
        self.package.plan().installer.frontend
    }

    pub fn payload_source(
        &self,
        cache: ContentCache,
    ) -> Result<AcquiredPayloadSource, PayloadError> {
        Ok(AcquiredPayloadSource::new(
            cache,
            self.catalog.clone(),
            &self.package,
        ))
    }

    pub fn prerequisite_bytes(
        &self,
        source: &AcquiredPayloadSource,
        digest: &Sha256Digest,
    ) -> Result<Vec<u8>, PayloadError> {
        source.read_blob(digest)
    }

    pub fn plugin_aot(
        &self,
        source: &AcquiredPayloadSource,
        digest: &Sha256Digest,
    ) -> Result<Vec<u8>, PayloadError> {
        source.read_blob(digest)
    }
}

pub fn open_cache(root: &Path, policy: CachePolicy) -> Result<ContentCache, HandoffRejection> {
    Ok(ContentCache::open(root, policy)?)
}

pub fn write_handoff(
    path: &Path,
    handoff: &RuntimeHandoff,
) -> Result<Sha256Digest, HandoffRejection> {
    let bytes = handoff
        .encode()
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_durable(path, &bytes)?;
    Ok(handoff.digest())
}

pub fn frontend(executable: &Path) -> Result<Option<Frontend>, HandoffRejection> {
    Executable::read(executable)
        .map(|executable| executable.program())
        .map(|program| match program {
            Some(ProgramKind::Windowed) => Some(Frontend::Gui),
            Some(ProgramKind::Console) => Some(Frontend::Console),
            None => None,
        })
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))
}
