use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use zup_acquire::{
    CachePolicy, ContentCache, ContentCatalog, ReleaseDescriptor, RuntimeHandoff, Verify,
};
use zup_binary::{Executable, ProgramKind};
use zup_bundle::{AcquiredPayloadSource, Package, PayloadError};
use zup_core::{Frontend, Sha256Digest};

use crate::bundle_packager::EmbeddedBundle;
use crate::durable::write_durable;

#[derive(Debug, thiserror::Error)]
pub enum HandoffRejection {
    #[error("the handoff document could not be read: {0}")]
    Unreadable(String),
    #[error("the handoff document is not a handoff: {0}")]
    Malformed(#[from] zup_acquire::HandoffError),
    #[error(
        "this executable is {found}, but the release document names {expected} for the selected variant"
    )]
    NotTheNamedRuntime {
        expected: Sha256Digest,
        found: Sha256Digest,
    },
    #[error("the release document {release} does not carry variant `{variant}`")]
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
    #[error("the handoff names catalog {handoff} but the release names {release}")]
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
    #[error("the release document does not carry a native runtime for `{0}`")]
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

pub struct HandoffContent {
    pub handoff: RuntimeHandoff,
    pub release: ReleaseDescriptor,
    pub catalog: ContentCatalog,
    pub manifest: Vec<u8>,
}

impl std::fmt::Debug for HandoffContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HandoffContent")
            .field("handoff", &self.handoff)
            .field("release", &self.release.release_digest)
            .field("catalog", &self.catalog.blobs.len())
            .field("manifest_bytes", &self.manifest.len())
            .finish()
    }
}

pub fn verify_handoff_content(
    executable: &Path,
    cache: &ContentCache,
    handoff: &RuntimeHandoff,
) -> Result<HandoffContent, HandoffRejection> {
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

    if runtime.size > zup_acquire::MAX_RUNTIME_BYTES {
        return Err(HandoffRejection::Unreadable(
            "the selected runtime exceeds the native image size limit".into(),
        ));
    }
    let file = std::fs::File::open(executable)?;
    if file.metadata()?.len() != runtime.size {
        return Err(HandoffRejection::Unreadable(
            "the source runtime length differs from the selected release".into(),
        ));
    }
    let (size, found) = zup_core::hash_reader(file.take(runtime.size.saturating_add(1)))?;
    if size != runtime.size || found != runtime.digest {
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

    Ok(HandoffContent {
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
    package: Package,
    catalog: ContentCatalog,
}

impl std::fmt::Debug for AcquiredBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcquiredBundle")
            .field("package_blobs", &self.package.blob_count())
            .field("catalog", &self.catalog.blobs.len())
            .finish()
    }
}

impl AcquiredBundle {
    pub fn validate_recorded_source(
        &self,
        content: &HandoffContent,
        expected: &zup_core::ReleaseIdentity,
    ) -> Result<(), HandoffRejection> {
        let installer = &self.package.plan().installer;
        let config = installer.updates.as_ref().ok_or_else(|| {
            HandoffRejection::Unreadable(
                "recorded source has no embedded update trust configuration".into(),
            )
        })?;
        if expected.validate().is_err()
            || expected.app_id != installer.app.id
            || expected.version != installer.app.version.to_string()
            || expected.target != installer.target
            || expected.frontend != installer.frontend.as_str()
            || expected.runtime != Some(content.handoff.runtime)
            || expected.document != content.handoff.document
            || expected.release != content.release.release_digest
            || expected.catalog != content.handoff.catalog
            || expected.variant != content.handoff.variant
            || expected.manifest != content.handoff.manifest
            || expected.trust_anchor != zup_core::hash_bytes(&config.trusted_root)
            || expected.channel != content.release.channel
            || expected.repository != content.handoff.repository
            || expected.pin != content.handoff.pin
            || expected.components
                != installer
                    .components
                    .iter()
                    .map(|component| component.id.to_string())
                    .collect::<Vec<_>>()
        {
            return Err(HandoffRejection::Unreadable(
                "source does not match the release authorized by the recovery journal".into(),
            ));
        }
        Ok(())
    }

    pub async fn authenticate(
        &self,
        handoff: &RuntimeHandoff,
        state_root: &Path,
    ) -> Result<zup_update::ResolvedRelease, HandoffRejection> {
        let installer = &self.package.plan().installer;
        let config = installer.updates.as_ref().ok_or_else(|| {
            HandoffRejection::Unreadable(
                "the executing runtime has no embedded update trust configuration".into(),
            )
        })?;
        self.authenticate_with_trust(handoff, state_root, config, None, None)
            .await
    }

    pub(crate) async fn authenticate_with_trust(
        &self,
        handoff: &RuntimeHandoff,
        state_root: &Path,
        config: &zup_core::UpdateConfig,
        peer: Option<&crate::peer_access::PeerAccess>,
        machine: Option<&crate::machine_authority::MachineRoot>,
    ) -> Result<zup_update::ResolvedRelease, HandoffRejection> {
        let installer = &self.package.plan().installer;
        if installer
            .updates
            .as_ref()
            .is_none_or(|selected| selected.trusted_root != config.trusted_root)
        {
            return Err(HandoffRejection::Unreadable("the selected runtime changes the embedded update trust root; an authorized trust transition is required".into()));
        }
        if match &handoff.pin {
            zup_core::ReleasePin::Release { version, document } => {
                *version != installer.app.version || *document != handoff.document
            }
            zup_core::ReleasePin::Channel { channel } => channel != &config.channel,
        } {
            return Err(HandoffRejection::Unreadable(
                "release pin differs from the executing runtime's application contract".into(),
            ));
        }
        let mut context = zup_update::TrustContext::from_update_config(
            config,
            installer.app.id.clone(),
            installer.install.scope,
            state_root,
        );
        context.trust.repository = handoff.repository.clone();
        context.trust.pin = handoff.pin.clone();
        context
            .trust
            .validate()
            .map_err(|detail| HandoffRejection::Unreadable(detail.into()))?;
        let resolver = zup_update::ReleaseResolver::new(
            context,
            zup_acquire::HostProfile::native().with_frontend(installer.frontend.as_str()),
            CachePolicy::Auto,
            zup_acquire::ProgressSink::discard(),
        )
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        let resolver = if let Some(machine) = machine {
            let machine = machine.clone();
            let context = resolver.context().clone();
            let store = tokio::task::spawn_blocking(move || machine.repository_store(&context))
                .await
                .map_err(|error| HandoffRejection::Unreadable(error.to_string()))??;
            resolver.with_metadata_store(store)
        } else {
            resolver
        };
        let resolved = match peer {
            Some(peer) => {
                resolver
                    .resolve_with_transport(crate::peer_access::PeerTransport {
                        peer: peer.clone(),
                        network: tough::DefaultTransport::default(),
                    })
                    .await
            }
            None => resolver.resolve().await,
        }
        .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        if resolved.descriptor_digest != handoff.document
            || resolved.descriptor.version != installer.app.version.to_string()
            || resolved.descriptor.release_digest != handoff.release
            || resolved.descriptor.catalog.digest != handoff.catalog
            || resolved.variant.id != handoff.variant
            || resolved.variant.manifest.digest != handoff.manifest
            || resolved
                .variant
                .runtime
                .is_none_or(|runtime| runtime.digest != handoff.runtime)
            || resolved.variant.target != installer.target
            || resolved.variant.frontend != installer.frontend.as_str()
        {
            return Err(HandoffRejection::Unreadable(
                "TUF-authenticated release differs from the requested handoff".into(),
            ));
        }
        Ok(resolved)
    }

    pub fn open(executable: &Path, verified: &HandoffContent) -> Result<Self, HandoffRejection> {
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
                "the variant manifest and this executable's embedded plan differ".into(),
            ));
        }
        Ok(Self {
            package,
            catalog: verified.catalog.clone(),
        })
    }
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

pub struct AcquiredRuntime {
    path: PathBuf,
    _source: crate::filesystem::PinnedSource,
    _stage: tempfile::TempDir,
    manifest: zup_artifact::VariantManifest,
    descriptor: zup_acquire::ContentDescriptor,
}

impl AcquiredRuntime {
    pub fn stage(acquired: &zup_update::content::Acquired) -> Result<Self, HandoffRejection> {
        let descriptor = acquired
            .resolved
            .require_runtime()
            .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        let blob = acquired
            .cache
            .get(&descriptor, Verify::Full)?
            .ok_or_else(|| {
                HandoffRejection::Unreadable(
                    "the authenticated runtime is missing from the content cache".into(),
                )
            })?;
        let stage = tempfile::Builder::new().prefix("zup-source-").tempdir()?;
        let root = stage.path().join("content");
        let directory = private_fs::PrivateDir::ensure(&root)
            .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        let path = root.join("runtime.exe");
        {
            let mut output = directory
                .create("runtime.exe")
                .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
            std::io::copy(&mut blob.open()?, &mut output)?;
            output.flush()?;
            output
                .sync_all()
                .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        }
        drop(directory);
        let mut source = crate::filesystem::pin_source(&path)
            .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
        let observed = zup_core::hash_reader(&mut source.file)?;
        if observed != (descriptor.size, descriptor.digest) {
            return Err(HandoffRejection::Unreadable(
                "staged runtime differs from the authenticated release".into(),
            ));
        }
        source.file.seek(SeekFrom::Start(0))?;
        let bundle = EmbeddedBundle::open(&path)?;
        if !bundle.package().is_plan_only() || bundle.plan() != &acquired.manifest.plan {
            return Err(HandoffRejection::Unreadable(
                "the authenticated runtime does not embed the selected application's plan".into(),
            ));
        }
        if bundle
            .plan()
            .installer
            .updates
            .as_ref()
            .is_none_or(|config| {
                zup_core::hash_bytes(&config.trusted_root) != acquired.resolved.trust_anchor
            })
        {
            return Err(HandoffRejection::Unreadable("the selected runtime changes the authenticated update trust root; an authorized trust transition is required".into()));
        }
        Ok(Self {
            path,
            _source: source,
            _stage: stage,
            manifest: acquired.manifest.clone(),
            descriptor,
        })
    }

    pub(crate) fn payload_source(
        &self,
        payload: std::sync::Arc<AcquiredPayloadSource>,
    ) -> AcquiredDeploymentSource {
        AcquiredDeploymentSource::new(
            payload,
            self.path.clone(),
            self.descriptor,
            self.manifest.clone(),
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) struct AcquiredDeploymentSource {
    payload: std::sync::Arc<AcquiredPayloadSource>,
    image: PathBuf,
    runtime: zup_acquire::ContentDescriptor,
    manifest: zup_artifact::VariantManifest,
}

impl AcquiredDeploymentSource {
    pub(crate) fn new(
        payload: std::sync::Arc<AcquiredPayloadSource>,
        image: PathBuf,
        runtime: zup_acquire::ContentDescriptor,
        manifest: zup_artifact::VariantManifest,
    ) -> Self {
        Self {
            payload,
            image,
            runtime,
            manifest,
        }
    }
}

impl zup_bundle::PayloadSource for AcquiredDeploymentSource {
    fn open(
        &self,
        path: &zup_core::RelativePath,
        digest: &Sha256Digest,
        size: u64,
    ) -> Result<zup_bundle::PayloadReader, PayloadError> {
        if path.as_str() == "__zup_maintenance__.exe" {
            if (self.runtime.digest, self.runtime.size) != (*digest, size) {
                return Err(PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            let mut file =
                std::fs::File::open(&self.image).map_err(|source| PayloadError::Read {
                    path: path.to_string(),
                    source,
                })?;
            let observed =
                zup_core::hash_reader(&mut file).map_err(|source| PayloadError::Read {
                    path: path.to_string(),
                    source,
                })?;
            if observed != (size, *digest) {
                return Err(PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            file.seek(SeekFrom::Start(0))
                .map_err(|source| PayloadError::Read {
                    path: path.to_string(),
                    source,
                })?;
            return Ok(Box::new(file));
        }
        let native = if path.as_str() == zup_bundle::PRESET_SOURCE {
            self.manifest
                .preset
                .as_ref()
                .map(|preset| (preset.digest, preset.size))
        } else if let Some(name) = path
            .as_str()
            .strip_prefix(zup_bundle::ASSET_SOURCE_PREFIX)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            self.manifest
                .plan
                .ui_assets
                .iter()
                .find(|asset| asset.name.as_str() == name)
                .map(|asset| (asset.sha256, asset.size))
        } else {
            return self.payload.open(path, digest, size);
        };
        if native != Some((*digest, size)) {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        Ok(Box::new(std::io::Cursor::new(
            self.payload.read_blob(digest)?,
        )))
    }
}

pub(crate) struct AuthenticatedSource {
    pub image: PathBuf,
    pub bundle: EmbeddedBundle,
    pub manifest: zup_artifact::VariantManifest,
    pub payload: std::sync::Arc<AcquiredPayloadSource>,
    pub runtime: zup_acquire::ContentDescriptor,
    pub release: zup_core::ReleaseIdentity,
    _pin: crate::filesystem::PinnedSource,
}

impl AuthenticatedSource {
    pub async fn open(
        source: &zup_protocol::AcquiredSource,
        cache_root: &Path,
        state_root: &Path,
        authority: &EmbeddedBundle,
        peer: &crate::peer_access::PeerAccess,
        machine: Option<&crate::machine_authority::MachineRoot>,
    ) -> Result<Self, HandoffRejection> {
        let handoff = &source.handoff;
        handoff.validate()?;
        let installer = &authority.plan().installer;
        if handoff.app_id != installer.app.id || handoff.target != installer.target {
            return Err(HandoffRejection::Unreadable(
                "selected source exceeds the worker's embedded application authority".into(),
            ));
        }
        let image = PathBuf::from(&source.image);
        let (pin, cache, verified, acquired, manifest, bundle) = {
            let _guard = peer.enter()?;
            let pin = crate::filesystem::pin_source(&image)
                .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
            let cache = ContentCache::open(cache_root, CachePolicy::Auto)?;
            let verified = verify_handoff_content(&image, &cache, handoff)?;
            let acquired = AcquiredBundle::open(&image, &verified)?;
            let manifest = zup_artifact::VariantManifest::parse(&verified.manifest)
                .map_err(|error| HandoffRejection::Unreadable(error.to_string()))?;
            let bundle = EmbeddedBundle::open(&image)?;
            (pin, cache, verified, acquired, manifest, bundle)
        };
        let config = installer.updates.as_ref().ok_or_else(|| {
            HandoffRejection::Unreadable("worker has no embedded update trust configuration".into())
        })?;
        let release = acquired
            .authenticate_with_trust(handoff, state_root, config, Some(peer), machine)
            .await?
            .identity(&manifest.plan.installer.components);
        let descriptor = verified
            .release
            .variant(&handoff.variant)
            .and_then(|variant| variant.runtime)
            .ok_or_else(|| HandoffRejection::NoRuntime(handoff.variant.clone()))?;
        let runtime = zup_acquire::ContentDescriptor::stored(
            zup_acquire::ContentKind::Runtime,
            descriptor.digest,
            descriptor.size,
        );
        let payload = std::sync::Arc::new(AcquiredPayloadSource::new(
            cache,
            acquired.catalog,
            &acquired.package,
        ));
        Ok(Self {
            image,
            bundle,
            manifest,
            payload,
            runtime,
            release,
            _pin: pin,
        })
    }
}
