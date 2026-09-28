//! Windows executable packaging and inspection.

use std::{
    fs::File,
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};

use thiserror::Error;
use zup_bundle::{
    AutoPayloadSource as PortableAutoPayloadSource, BundleWriter, CompiledPluginArtifact,
    DirectoryPayloadSource, Package, PackageError, PackagePayloadSource, PayloadError,
    PayloadReader, PayloadSource,
};
use zup_core::TargetBuildPlan;
use zup_core::{
    Frontend, PLUGIN_PAYLOAD_ROOT, RelativePath, Sha256Digest, TargetTriple, hash_reader,
};
use zup_pe::{
    MAX_RESOURCE_SIZE, PeError, RESOURCE_ID_BLOB_START, RESOURCE_ID_INDEX, ResourceDocument,
};

const WINDOWS_X64_TARGET: &str = "x86_64-pc-windows-msvc";
const WINDOWS_ARM64_TARGET: &str = "aarch64-pc-windows-msvc";

/// Errors produced by the Windows package adapter.
#[derive(Debug, Error)]
pub enum BundleError {
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Portable(PeError),
    #[error("bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundle is truncated, corrupt, or unsupported")]
    Invalid,
    #[error("executable has no embedded package index")]
    MissingResource,
    #[error("bare runtime has no content store beside it at `{path}`")]
    MissingSidecar { path: String },
    #[error("`{path}` is a universal artifact; run it to install the variant this host needs")]
    UniversalArtifact { path: String },
    #[error("package data is {size} bytes; the Windows resource limit is {limit} bytes")]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("package has {count} blobs; the Windows resource identifier limit is {limit}")]
    TooManyBlobs { count: usize, limit: usize },
    #[error("cannot allocate {size} bytes while processing executable resources")]
    ResourceAllocation { size: u64 },
    #[error("image resources: {0}")]
    Resource(String),
    #[error("runtime already has an Authenticode certificate table; embed before signing")]
    RuntimeAlreadySigned,
    #[error("runtime frontend is {found:?}; expected {expected:?}")]
    FrontendMismatch { expected: Frontend, found: Frontend },
    #[error("target mismatch: expected `{expected}`, found `{found}`")]
    TargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },
}

impl BundleError {
    pub fn is_missing_resource(&self) -> bool {
        matches!(self, Self::MissingResource)
    }
}

impl From<PeError> for BundleError {
    fn from(error: PeError) -> Self {
        match error {
            PeError::Invalid => Self::Invalid,
            other => Self::Portable(other),
        }
    }
}

/// Read one embedded resource, mapping a missing resource to the adapter's own
/// answer so a program without a package and a program with a broken one are
/// told apart.
fn read_resource(executable: &Path, id: usize) -> Result<Vec<u8>, BundleError> {
    crate::pe_resources::read_resource(executable, id).map_err(|error| match error {
        crate::pe_resources::ResourceError::Absent(_) => BundleError::MissingResource,
        crate::pe_resources::ResourceError::Pe(error) => BundleError::Portable(error),
        other => BundleError::Resource(other.to_string()),
    })
}

/// A package embedded in a Windows executable.
#[derive(Debug, Clone)]
pub struct EmbeddedBundle {
    executable: PathBuf,
    package: Package,
}

impl EmbeddedBundle {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let executable = path.as_ref().to_path_buf();
        let index = read_resource(&executable, RESOURCE_ID_INDEX)?;
        let index_info = Package::parse_index(&index)?;
        if index_info.blob_count() > zup_pe::MAX_RESOURCE_ID {
            return Err(BundleError::TooManyBlobs {
                count: index_info.blob_count(),
                limit: zup_pe::MAX_RESOURCE_ID,
            });
        }
        let mut blobs = Vec::new();
        blobs
            .try_reserve_exact(index_info.blob_count())
            .map_err(|_| BundleError::ResourceAllocation {
                size: index_info.blob_count() as u64,
            })?;
        let mut total_size = index.len() as u64;
        for index in 0..index_info.blob_count() {
            let expected = index_info
                .compressed_size(index)
                .ok_or(BundleError::Invalid)?;
            if expected > MAX_RESOURCE_SIZE {
                return Err(BundleError::ResourceTooLarge {
                    size: expected,
                    limit: MAX_RESOURCE_SIZE,
                });
            }
            let blob = read_resource(&executable, index + RESOURCE_ID_BLOB_START)?;
            if blob.len() as u64 != expected {
                return Err(BundleError::Invalid);
            }
            total_size = total_size
                .checked_add(blob.len() as u64)
                .ok_or(BundleError::Invalid)?;
            blobs.push(blob);
        }
        let total_size = usize::try_from(total_size).map_err(|_| BundleError::Invalid)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total_size)
            .map_err(|_| BundleError::ResourceAllocation {
                size: total_size as u64,
            })?;
        bytes.extend_from_slice(&index);
        for blob in blobs {
            bytes.extend_from_slice(&blob);
        }
        let package = Package::from_bytes(bytes)?;
        Ok(Self {
            executable,
            package,
        })
    }

    pub fn package(&self) -> &Package {
        &self.package
    }

    pub fn package_payload_source(&self) -> PackagePayloadSource {
        self.package.payload_source()
    }

    pub fn plan(&self) -> &zup_bundle::PortableBuildPlan {
        self.package.plan()
    }

    pub fn target(&self) -> &TargetTriple {
        &self.package.plan().installer.target
    }

    pub fn build_plan(&self) -> Result<zup_core::BuildPlan, BundleError> {
        Ok(self.package.build_plan()?)
    }

    pub fn frontend(&self) -> Frontend {
        self.plan().installer.frontend
    }

    pub fn plugin_artifacts(&self) -> &[zup_bundle::PluginArtifact] {
        self.package.plugin_artifacts()
    }

    pub fn plugin_artifact(&self, id: &zup_core::PluginId) -> Option<&zup_bundle::PluginArtifact> {
        self.package.plugin_artifact(id)
    }

    pub fn plugin_aot(&self, id: &zup_core::PluginId) -> Result<Vec<u8>, BundleError> {
        Ok(self.package.plugin_aot(id)?)
    }

    pub fn prerequisite_artifact(
        &self,
        id: &zup_core::PrerequisiteId,
    ) -> Option<&zup_bundle::PrerequisiteArtifact> {
        self.package.prerequisite_artifact(id)
    }

    pub fn prerequisite_bytes(
        &self,
        id: &zup_core::PrerequisiteId,
    ) -> Result<Vec<u8>, BundleError> {
        Ok(self.package.prerequisite_bytes(id)?)
    }

    pub fn payload_source(&self) -> EmbeddedPayloadSource {
        EmbeddedPayloadSource {
            executable: self.executable.clone(),
            package: self.package.payload_source(),
        }
    }
}

impl PayloadSource for EmbeddedBundle {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        self.payload_source()
            .open(path, expected_sha256, expected_size)
    }
}

#[derive(Clone)]
pub struct EmbeddedPayloadSource {
    executable: PathBuf,
    package: PackagePayloadSource,
}

impl EmbeddedPayloadSource {
    fn open_maintenance(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        let mut file = File::open(&self.executable).map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        let (size, digest) = hash_reader(&mut file).map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        if size != expected_size {
            return Err(PayloadError::SizeMismatch {
                path: path.to_string(),
                expected: expected_size,
                found: size,
            });
        }
        if digest != *expected_sha256 {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|source| PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        Ok(Box::new(file))
    }
}

impl PayloadSource for EmbeddedPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        if path.as_str() == "__zup_maintenance__.exe" {
            return self.open_maintenance(path, expected_sha256, expected_size);
        }
        self.package.open(path, expected_sha256, expected_size)
    }
}

/// Selects directory, standalone-package, or embedded-executable payload data.
///
/// A native runtime's content may live in its own resources, in a standalone
/// package file, or in a **sidecar store** beside the executable. The sidecar is
/// what a selected universal variant installs as, and what an installed
/// maintenance copy reads from after the original artifact is gone. Which one is
/// in play is decided here and is invisible above: the lifecycle planner only
/// ever asks for a portable path, a digest, and a length.
pub enum AutoPayloadSource {
    Portable(PortableAutoPayloadSource),
    Embedded(EmbeddedPayloadSource),
    Overlay(OverlayPayloadSource),
}

impl AutoPayloadSource {
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, BundleError> {
        let path = path.into();
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_dir() {
            return Ok(Self::Portable(
                PortableAutoPayloadSource::from_path(path).map_err(BundleError::from)?,
            ));
        }
        match PortableAutoPayloadSource::from_path(&path) {
            Ok(source) => Ok(Self::Portable(source)),
            Err(_package_error) if looks_like_pe(&path) => match EmbeddedBundle::open(&path) {
                Ok(bundle) => Ok(Self::Embedded(bundle.payload_source())),
                // An image that carries a universal artifact is not a payload
                // root; it is something to run.
                Err(error) if error.is_missing_resource() => {
                    if is_universal_artifact(&path) {
                        return Err(BundleError::UniversalArtifact {
                            path: path.display().to_string(),
                        });
                    }
                    // An image with no package of its own is a bare native
                    // runtime, whose content is the sidecar store the
                    // installation persisted beside it.
                    Self::from_sidecar(&path)
                }
                // An image that does carry something zup wrote but cannot read is
                // a defect, not a sidecar case.
                Err(error) => Err(error),
            },
            Err(package_error) => Err(BundleError::Package(package_error)),
        }
    }

    /// Read the sidecar store beside `executable`, if there is one.
    ///
    /// A missing sidecar beside a bare runtime is a real refusal: the runtime has
    /// no content and no way to get any.
    fn from_sidecar(executable: &Path) -> Result<Self, BundleError> {
        let sidecar = sidecar_package_path(executable);
        if !sidecar.is_file() {
            return Err(BundleError::MissingSidecar {
                path: sidecar.display().to_string(),
            });
        }
        let package = Package::open(&sidecar)?;
        Ok(Self::Portable(PortableAutoPayloadSource::Package(
            package.payload_source(),
        )))
    }

    pub fn from_paths(
        payload_root: impl Into<PathBuf>,
        payload_overlay_root: Option<PathBuf>,
    ) -> Result<Self, BundleError> {
        let base = Self::from_path(payload_root)?;
        Ok(Self::Overlay(OverlayPayloadSource::new(
            base,
            payload_overlay_root,
        )))
    }
}

/// The sidecar package path beside an executable.
pub fn sidecar_package_path(executable: &Path) -> PathBuf {
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(crate::content_store::MAINTENANCE_PACKAGE_NAME)
}

/// Whether an image is a universal artifact rather than a payload root.
///
/// The two are told apart by their first resource, which is a package index in
/// one case and an artifact index in the other. A single-target artifact stays a
/// payload root, so this is a narrow question with a narrow answer.
fn is_universal_artifact(executable: &Path) -> bool {
    match crate::pe_resources::read_resource(executable, RESOURCE_ID_INDEX) {
        Ok(bytes) => zup_artifact::ArtifactIndex::parse(&bytes).is_ok(),
        Err(_) => false,
    }
}

impl PayloadSource for AutoPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        match self {
            Self::Overlay(source) => source.open(path, expected_sha256, expected_size),
            Self::Portable(source) => {
                if is_plugin_path(path) {
                    Err(PayloadError::NotFound {
                        path: path.to_string(),
                    })
                } else {
                    source.open(path, expected_sha256, expected_size)
                }
            }
            Self::Embedded(source) => {
                if is_plugin_path(path) {
                    Err(PayloadError::NotFound {
                        path: path.to_string(),
                    })
                } else {
                    source.open(path, expected_sha256, expected_size)
                }
            }
        }
    }
}

pub struct OverlayPayloadSource {
    base: Box<AutoPayloadSource>,
    overlay: Option<DirectoryPayloadSource>,
}

impl OverlayPayloadSource {
    pub fn new(base: AutoPayloadSource, overlay: Option<PathBuf>) -> Self {
        Self {
            base: Box::new(base),
            overlay: overlay.map(DirectoryPayloadSource::new),
        }
    }
}

impl PayloadSource for OverlayPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        if is_plugin_path(path) {
            return match &self.overlay {
                Some(overlay) => overlay.open(path, expected_sha256, expected_size),
                None => Err(PayloadError::NotFound {
                    path: path.to_string(),
                }),
            };
        }
        if let Some(overlay) = &self.overlay {
            match overlay.open(path, expected_sha256, expected_size) {
                Ok(reader) => return Ok(reader),
                Err(PayloadError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        self.base.open(path, expected_sha256, expected_size)
    }
}

fn is_plugin_path(path: &RelativePath) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component == PLUGIN_PAYLOAD_ROOT)
}

fn looks_like_pe(path: &Path) -> bool {
    zup_pe::looks_like_pe(path)
}

/// Build an installer executable containing a package index and its blobs.
pub fn build_self_contained_executable(
    executable: &Path,
    output: &Path,
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<(u64, u64), BundleError> {
    let runtime_target = read_pe_target(executable)?;
    if runtime_target != plan.installer.target {
        return Err(BundleError::TargetMismatch {
            expected: plan.installer.target.clone(),
            found: runtime_target,
        });
    }
    validate_pe_frontend(executable, plan.installer.frontend)?;
    validate_unsigned_pe(executable)?;
    let temporary = tempfile::tempdir()?;
    let package = temporary.path().join("installer.zup");
    let package_size = BundleWriter::write_file(plan, artifacts, &package)?;
    embed_bundle_file(executable, output, &package)?;
    Ok((std::fs::metadata(output)?.len(), package_size))
}

/// Build a **thin** native runtime: the plan, and none of the content.
///
/// This is the executable a thin installer fetches and hands control to. It
/// knows exactly what it would install — application identity, components, file
/// destinations, prerequisites, plugin bindings — so it can plan and execute a
/// lifecycle with no manifest, and it holds none of the bytes, because the bytes
/// come from a verified content-addressed cache the release graph
/// authenticated.
///
/// The alternative — embedding the payload — is what an offline artifact does,
/// and it makes the runtime the entire application. Which is the reason a thin
/// installer built that way would not be thin.
pub fn build_plan_only_executable(
    executable: &Path,
    output: &Path,
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<(u64, u64), BundleError> {
    let (bytes, package_size) = plan_only_runtime_bytes(executable, plan, artifacts)?;
    std::fs::write(output, &bytes)?;
    Ok((bytes.len() as u64, package_size))
}

/// The plan-only runtime image, as bytes.
///
/// The bytes are the useful shape for a publisher: the image is content a
/// release graph names, so it has to exist as bytes before anything writes a
/// file, and a caller that wrote a temporary file first would have two
/// processes racing over one name.
pub fn plan_only_runtime_bytes(
    executable: &Path,
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<(Vec<u8>, u64), BundleError> {
    let runtime_target = read_pe_target(executable)?;
    if runtime_target != plan.installer.target {
        return Err(BundleError::TargetMismatch {
            expected: plan.installer.target.clone(),
            found: runtime_target,
        });
    }
    validate_pe_frontend(executable, plan.installer.frontend)?;
    validate_unsigned_pe(executable)?;
    let package = BundleWriter::encode_plan_only(plan, artifacts)?;
    let package_size = package.len() as u64;
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join("runtime.zup");
    std::fs::write(&path, &package)?;
    let out = temporary.path().join("runtime.exe");
    embed_bundle_file(executable, &out, &path)?;
    Ok((std::fs::read(&out)?, package_size))
}

/// Embed a prebuilt portable package in an executable.
pub fn embed_bundle_file(
    executable: &Path,
    output: &Path,
    package: &Path,
) -> Result<(), BundleError> {
    validate_unsigned_pe(executable)?;
    embed_bundle_resource(executable, output, package)
}

pub fn read_pe_target(path: &Path) -> Result<TargetTriple, BundleError> {
    let target = match zup_pe::read_pe_header(path)?.machine {
        zup_pe::Machine::Amd64 => WINDOWS_X64_TARGET,
        zup_pe::Machine::Arm64 => WINDOWS_ARM64_TARGET,
        _ => return Err(BundleError::Invalid),
    };
    TargetTriple::parse(target).map_err(|_| BundleError::Invalid)
}

pub fn validate_embedded_bundle_target(
    bundle: &EmbeddedBundle,
    expected: &TargetTriple,
) -> Result<(), BundleError> {
    if bundle.target() != expected {
        return Err(BundleError::TargetMismatch {
            expected: expected.clone(),
            found: bundle.target().clone(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeSubsystem {
    Console,
    Gui,
}

pub fn read_pe_subsystem(path: &Path) -> Result<PeSubsystem, BundleError> {
    match zup_pe::read_pe_header(path)?.subsystem {
        zup_pe::Subsystem::Console => Ok(PeSubsystem::Console),
        zup_pe::Subsystem::Gui => Ok(PeSubsystem::Gui),
        zup_pe::Subsystem::Other(_) => Err(BundleError::Invalid),
    }
}

pub fn read_pe_frontend(path: &Path) -> Result<Frontend, BundleError> {
    match read_pe_subsystem(path)? {
        PeSubsystem::Console => Ok(Frontend::Console),
        PeSubsystem::Gui => Ok(Frontend::Gui),
    }
}

pub fn validate_pe_frontend(path: &Path, expected: Frontend) -> Result<(), BundleError> {
    let found = read_pe_frontend(path)?;
    let matches = match expected {
        Frontend::Gui => found == Frontend::Gui,
        Frontend::Console | Frontend::Headless => found == Frontend::Console,
    };
    if !matches {
        return Err(BundleError::FrontendMismatch { expected, found });
    }
    Ok(())
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    if zup_pe::is_signed(path)? {
        return Err(BundleError::RuntimeAlreadySigned);
    }
    Ok(())
}

/// The resource documents a portable package occupies: the index, then one
/// resource per compressed blob.
fn package_documents(package: &Package) -> Result<Vec<ResourceDocument>, BundleError> {
    let index = package.index_bytes()?;
    if index.len() as u64 > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size: index.len() as u64,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    if package.blob_count() > zup_pe::MAX_RESOURCE_ID {
        return Err(BundleError::TooManyBlobs {
            count: package.blob_count(),
            limit: zup_pe::MAX_RESOURCE_ID,
        });
    }
    let mut documents = Vec::with_capacity(package.blob_count() + 1);
    documents.push(ResourceDocument {
        id: RESOURCE_ID_INDEX,
        bytes: index,
    });
    for index in 0..package.blob_count() {
        if package.index_info().compressed_size(index).unwrap_or(0) > MAX_RESOURCE_SIZE {
            return Err(BundleError::ResourceTooLarge {
                size: package.index_info().compressed_size(index).unwrap_or(0),
                limit: MAX_RESOURCE_SIZE,
            });
        }
        documents.push(ResourceDocument {
            id: index + RESOURCE_ID_BLOB_START,
            bytes: package.compressed_blob(index)?,
        });
    }
    Ok(documents)
}

fn embed_bundle_resource(
    executable: &Path,
    output: &Path,
    package_path: &Path,
) -> Result<(), BundleError> {
    let package = Package::open_unverified(package_path)?;
    let documents = package_documents(&package)?;
    crate::pe_resources::write_resources(executable, output, &documents).map_err(|error| {
        match error {
            crate::pe_resources::ResourceError::Pe(error) => BundleError::Portable(error),
            other => BundleError::Resource(other.to_string()),
        }
    })?;
    Ok(())
}
