use std::{
    fs::File,
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};
use zup_transaction::MAINTENANCE_PACKAGE_NAME;

use thiserror::Error;
use zup_binary::{BinaryFormat, Executable, ProgramKind};
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
    MAX_RESOURCE_SIZE, PeError, RESOURCE_ID_BLOB_START, RESOURCE_ID_INDEX, RESOURCE_ID_PRESET,
    ResourceDocument,
};

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
    #[error("target mismatch: expected `{expected}`, found `{found}`")]
    TargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },
    #[error(transparent)]
    Target(#[from] zup_binary::TargetRefusal),
    #[error(transparent)]
    Frontend(#[from] zup_binary::FrontendRefusal),
    #[error(transparent)]
    Inspect(#[from] zup_binary::InspectError),
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

fn read_resource(executable: &Path, id: usize) -> Result<Vec<u8>, BundleError> {
    crate::pe_resources::read_resource(executable, id).map_err(|error| match error {
        crate::pe_resources::ResourceError::Absent(_) => BundleError::MissingResource,
        crate::pe_resources::ResourceError::Pe(error) => BundleError::Portable(error),
        other => BundleError::Resource(other.to_string()),
    })
}

#[derive(Debug, Clone)]
pub struct EmbeddedBundle {
    executable: PathBuf,
    package: Package,

    preset: Option<Vec<u8>>,
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

        let preset = read_resource(&executable, RESOURCE_ID_PRESET).ok();
        Ok(Self {
            executable,
            package,
            preset,
        })
    }

    pub fn preset(&self) -> Option<&[u8]> {
        self.preset.as_deref()
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

    pub fn ui_asset(&self, name: &str) -> Result<(zup_core::PresetAsset, Vec<u8>), BundleError> {
        self.package
            .ui_asset(name)
            .map(|(asset, bytes)| (asset.clone(), bytes))
            .map_err(BundleError::from)
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

    fn open_preset(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        let bundle =
            EmbeddedBundle::open(&self.executable).map_err(|error| PayloadError::Read {
                path: path.to_string(),
                source: std::io::Error::other(error.to_string()),
            })?;
        let bytes = bundle
            .preset()
            .ok_or(PayloadError::Read {
                path: path.to_string(),
                source: std::io::Error::other("this image carries no preset"),
            })?
            .to_vec();
        let size = bytes.len() as u64;
        if size != expected_size {
            return Err(PayloadError::SizeMismatch {
                path: path.to_string(),
                expected: expected_size,
                found: size,
            });
        }
        if zup_core::hash_bytes(&bytes) != *expected_sha256 {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        Ok(Box::new(std::io::Cursor::new(bytes)))
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
        if path.as_str() == zup_bundle::PRESET_SOURCE {
            return self.open_preset(path, expected_sha256, expected_size);
        }
        if let Some(name) = path
            .as_str()
            .strip_prefix(zup_bundle::ASSET_SOURCE_PREFIX)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            let (asset, bytes) = EmbeddedBundle::open(&self.executable)
                .and_then(|bundle| bundle.ui_asset(name))
                .map_err(|error| PayloadError::Read {
                    path: path.to_string(),
                    source: std::io::Error::other(error.to_string()),
                })?;
            if bytes.len() as u64 != expected_size {
                return Err(PayloadError::SizeMismatch {
                    path: path.to_string(),
                    expected: expected_size,
                    found: bytes.len() as u64,
                });
            }
            if asset.sha256 != *expected_sha256 {
                return Err(PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            return Ok(Box::new(std::io::Cursor::new(bytes)));
        }
        self.package.open(path, expected_sha256, expected_size)
    }
}

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
            Err(_package_error) if is_executable_image(&path) => {
                match EmbeddedBundle::open(&path) {
                    Ok(bundle) => Ok(Self::Embedded(bundle.payload_source())),

                    Err(error) if error.is_missing_resource() => {
                        if is_universal_artifact(&path) {
                            return Err(BundleError::UniversalArtifact {
                                path: path.display().to_string(),
                            });
                        }

                        Self::from_sidecar(&path)
                    }

                    Err(error) => Err(error),
                }
            }
            Err(package_error) => Err(BundleError::Package(package_error)),
        }
    }

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

pub fn sidecar_package_path(executable: &Path) -> PathBuf {
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MAINTENANCE_PACKAGE_NAME)
}

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

pub fn build_self_contained_executable(
    executable: &Path,
    output: &Path,
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
    preset: Option<&[u8]>,
) -> Result<(u64, u64), BundleError> {
    validate_runtime_executable(executable, &plan.installer)?;
    validate_unsigned_pe(executable)?;
    let temporary = tempfile::tempdir()?;
    let package = temporary.path().join("installer.zup");
    let package_size = BundleWriter::write_file(plan, artifacts, &package)?;
    embed_bundle_file(executable, output, &package, preset)?;
    apply_plan_icon(output, plan)?;
    Ok((std::fs::metadata(output)?.len(), package_size))
}

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

pub fn plan_only_runtime_bytes(
    executable: &Path,
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<(Vec<u8>, u64), BundleError> {
    validate_runtime_executable(executable, &plan.installer)?;
    validate_unsigned_pe(executable)?;
    let package = BundleWriter::encode_plan_only(plan, artifacts)?;
    let package_size = package.len() as u64;
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join("runtime.zup");
    std::fs::write(&path, &package)?;
    let out = temporary.path().join("runtime.exe");
    embed_bundle_file(executable, &out, &path, None)?;
    apply_plan_icon(&out, plan)?;
    Ok((std::fs::read(&out)?, package_size))
}

pub fn embed_bundle_file(
    executable: &Path,
    output: &Path,
    package: &Path,
    preset: Option<&[u8]>,
) -> Result<(), BundleError> {
    validate_unsigned_pe(executable)?;
    embed_bundle_resource(executable, output, package, preset)
}

fn validate_runtime_executable(
    executable: &Path,
    installer: &zup_core::Installer,
) -> Result<(), BundleError> {
    let runtime = Executable::read(executable)?;
    if runtime.format() != BinaryFormat::Pe {
        return Err(BundleError::Invalid);
    }
    runtime.refuse_target(&installer.target)?;
    runtime.refuse_frontend(installer.frontend)?;
    Ok(())
}

pub fn is_executable_image(path: &Path) -> bool {
    Executable::read(path).is_ok_and(|executable| executable.format() == BinaryFormat::Pe)
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

pub fn read_frontend(path: &Path) -> Result<Option<Frontend>, BundleError> {
    let program = Executable::read(path)?.program();
    Ok(match program {
        Some(ProgramKind::Windowed) => Some(Frontend::Gui),
        Some(ProgramKind::Console) => Some(Frontend::Console),
        None => None,
    })
}

pub fn validate_frontend(path: &Path, expected: Frontend) -> Result<(), BundleError> {
    Executable::read(path)?.refuse_frontend(expected)?;
    Ok(())
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    if zup_pe::is_signed(path)? {
        return Err(BundleError::RuntimeAlreadySigned);
    }
    Ok(())
}

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

fn apply_plan_icon(output: &Path, plan: &TargetBuildPlan) -> Result<(), BundleError> {
    let Some(icon) = plan
        .icons
        .artifacts
        .iter()
        .find(|icon| icon.role == zup_core::IconRole::Windows)
    else {
        return Ok(());
    };
    if icon.executable_images.is_empty() || icon.executable_group.is_empty() {
        return Ok(());
    }
    crate::pe_resources::apply_icon(output, &icon.executable_images, &icon.executable_group)
        .map_err(|error| match error {
            crate::pe_resources::ResourceError::Pe(error) => BundleError::Portable(error),
            other => BundleError::Resource(other.to_string()),
        })
}

fn embed_bundle_resource(
    executable: &Path,
    output: &Path,
    package_path: &Path,
    preset: Option<&[u8]>,
) -> Result<(), BundleError> {
    let package = Package::open_unverified(package_path)?;
    let mut documents = package_documents(&package)?;
    if let Some(bytes) = preset {
        if bytes.len() as u64 > MAX_RESOURCE_SIZE {
            return Err(BundleError::ResourceTooLarge {
                size: bytes.len() as u64,
                limit: MAX_RESOURCE_SIZE,
            });
        }
        documents.push(ResourceDocument {
            id: RESOURCE_ID_PRESET,
            bytes: bytes.to_vec(),
        });
    }
    crate::pe_resources::write_resources(executable, output, &documents).map_err(|error| {
        match error {
            crate::pe_resources::ResourceError::Pe(error) => BundleError::Portable(error),
            other => BundleError::Resource(other.to_string()),
        }
    })?;
    Ok(())
}
