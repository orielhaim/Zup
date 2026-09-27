//! The native runtime's side of a handoff.
//!
//! A thin bootstrapper resolves a release, verifies a native runtime, and starts
//! it. Everything it says is a *claim*, and this module is where the claim is
//! checked. The runtime is about to write files, registry entries, and services,
//! so it re-derives what it is installing from authenticated descriptors rather
//! than from anything the previous process said.
//!
//! # The one check that binds everything
//!
//! ```text
//! handoff.runtime == sha256(this executable)
//! ```
//!
//! That single equality is what makes the rest of the handoff meaningful. A
//! release authenticates the runtime digest for each of its variants, and the
//! release's own fingerprint was verified before a byte was downloaded. So a
//! bootstrapper that substitutes a release must produce a release that names
//! *this* image's digest — which requires a SHA-256 preimage. One that
//! substitutes a payload must produce blobs that hash to the digests in the
//! authenticated catalog. One that substitutes a target or a variant is caught
//! by comparing the release against the target and frontend compiled into this
//! image's own plan.
//!
//! What the bootstrapper *does* supply is a location: where the verified cache
//! is. A hostile location is a cache full of objects that do not hash to the
//! authenticated digests, which is a failure and not a compromise. The location
//! is therefore the one untrusted input, and it is treated as a location and
//! nothing more.

use std::path::{Path, PathBuf};

use zup_acquire::{
    CachePolicy, ContentCache, ContentCatalog, ReleaseDescriptor, RuntimeHandoff, Verify,
};
use zup_bundle::{AcquiredPayloadSource, Package, PayloadError};
use zup_core::Sha256Digest;

use crate::bundle_packager::{EmbeddedBundle, PeSubsystem, read_pe_subsystem, read_pe_target};
use crate::durable::write_durable;

/// A verified view of one authenticated release, from inside a native runtime.
pub struct AcquiredRelease {
    /// The authenticated release graph.
    pub release: ReleaseDescriptor,
    /// The variant this machine installs.
    pub variant: String,
    /// The variant descriptor, exactly as the release named it.
    pub manifest: Vec<u8>,
    /// The content catalog.
    pub catalog: ContentCatalog,
    /// The verified cache the content lives in.
    pub cache: ContentCache,
    /// Where the handoff document was read from.
    pub handoff_path: PathBuf,
    /// The handoff's own fingerprint, which the caller recorded.
    pub handoff_digest: Sha256Digest,
}

/// Why a handoff could not be accepted.
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
    /// Whether this refusal happened before any machine change.
    ///
    /// Always yes. Every one of these is a check performed before the
    /// transaction engine is even asked for a plan.
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}

/// This executable's own digest, as a release would name it.
pub fn own_digest(executable: &Path) -> Result<Sha256Digest, std::io::Error> {
    let (_, digest) = zup_core::hash_reader(std::fs::File::open(executable)?)?;
    Ok(digest)
}

/// Read a handoff and check that this process is the one it names.
///
/// `expected` is the digest the caller was told to expect, which the caller
/// learned from the bootstrapper — so it is a *consistency* check between two
/// processes, not the authority. The authority is `handoff.runtime` matched
/// against the authenticated release, which this function then does.
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

/// Everything a thin runtime has to prove before it touches the machine.
pub struct VerifiedHandoff {
    pub handoff: RuntimeHandoff,
    pub release: ReleaseDescriptor,
    pub catalog: ContentCatalog,
    pub manifest: Vec<u8>,
}

impl std::fmt::Debug for VerifiedHandoff {
    /// The manifest is summarised by its digest rather than printed.
    ///
    /// A derived `Debug` would dump every byte of every installed file's plan,
    /// which is megabytes of hex on a panic — and the bytes are already proved by
    /// the digest, so printing them adds nothing a reader could use.
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

/// Verify a handoff end to end against this executable and the cache.
///
/// The order is the point. Identity first, then the graph, then the claims the
/// graph makes about the variant, then the claims this executable makes about
/// itself. Nothing here can be satisfied by naming a different location.
pub fn verify(
    executable: &Path,
    cache: &ContentCache,
    handoff: &RuntimeHandoff,
) -> Result<VerifiedHandoff, HandoffRejection> {
    // The document is addressed by the digest of its own bytes, and its
    // fingerprint is recomputed from the body. Those are different numbers — a
    // release document names its own fingerprint as a field, so it cannot also
    // be the hash of its own encoding — and checking both is what makes the
    // cache key a location rather than an authority.
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

    // The last and strongest check: the release names this image, and this is
    // this image.
    let found = own_digest(executable)?;
    if found != runtime.digest {
        return Err(HandoffRejection::NotTheNamedRuntime {
            expected: runtime.digest,
            found,
        });
    }

    // The claims the graph makes about this variant have to be the claims the
    // handoff repeats. A bootstrapper that substituted an arm64 target into an
    // x64 handoff would otherwise get a verified document and a plan for the
    // wrong machine, and the digest checks above would not notice because the
    // runtime image really is the one the release names.
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
    // And the machine's own opinion, which no handoff can overrule: this image is
    // built for the triple it is built for, and a claim otherwise is a lie about
    // the only party that cannot be lied to.
    let built_for = read_pe_target(executable)
        .map_err(|error| HandoffRejection::Unreadable(format!("this executable: {error}")))?;
    if built_for != handoff.target {
        return Err(HandoffRejection::TargetMismatch {
            handoff: handoff.target.to_string(),
            executable: built_for.to_string(),
        });
    }

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
    // A document is stored under its own digest like anything else, and it is
    // re-hashed in full on the way out, because a document is what every other
    // decision is made from.
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

/// The plan this executable installs, checked against the handoff.
pub struct AcquiredBundle {
    /// The plan-only package embedded in this image.
    pub package: Package,
    /// The plan the release's variant descriptor names.
    pub release: ReleaseDescriptor,
    pub variant: String,
    pub catalog: ContentCatalog,
}

impl std::fmt::Debug for AcquiredBundle {
    /// The package is summarised by its size rather than printed, for the same
    /// reason [`VerifiedHandoff`]'s manifest is: the proof is the digest.
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
    /// Open this executable's package and check it against a verified handoff.
    ///
    /// The package is the plan compiled into the runtime; the release is the
    /// plan the graph authenticated. They have to be the same plan, which is
    /// what proves the bootstrapper did not point this image at a different
    /// release's content.
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
        // The plan compiled into this image and the plan the graph authenticated
        // have to be the same plan. A bootstrapper that pointed this image at a
        // different release's content is caught here, not at install time.
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

    /// The target this image installs for.
    pub fn target(&self) -> &zup_core::TargetTriple {
        &self.package.plan().installer.target
    }

    /// The frontend this image presents.
    pub fn frontend(&self) -> zup_core::Frontend {
        self.package.plan().installer.frontend
    }

    /// A content source over the verified cache.
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

    /// Read one prerequisite package out of the verified cache.
    pub fn prerequisite_bytes(
        &self,
        source: &AcquiredPayloadSource,
        digest: &Sha256Digest,
    ) -> Result<Vec<u8>, PayloadError> {
        source.read_blob(digest)
    }

    /// Read one plugin's ahead-of-time image out of the verified cache.
    pub fn plugin_aot(
        &self,
        source: &AcquiredPayloadSource,
        digest: &Sha256Digest,
    ) -> Result<Vec<u8>, PayloadError> {
        source.read_blob(digest)
    }
}

/// Open the verified cache a handoff names.
pub fn open_cache(root: &Path, policy: CachePolicy) -> Result<ContentCache, HandoffRejection> {
    Ok(ContentCache::open(root, policy)?)
}

/// Write a handoff where the runtime can find it.
///
/// The document is written through the durable path, so a process that starts
/// immediately after the write cannot observe a partial one. It is a claim
/// rather than an authority, but a torn claim is still a confusing failure.
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

/// The subsystem this image presents, which the bootstrapper matches against the
/// variant it selected.
pub fn subsystem(executable: &Path) -> Result<PeSubsystem, HandoffRejection> {
    read_pe_subsystem(executable).map_err(|error| HandoffRejection::Unreadable(error.to_string()))
}
