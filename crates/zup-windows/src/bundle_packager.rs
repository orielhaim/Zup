//! Windows executable packaging and inspection.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use thiserror::Error;
use zup_build::TargetBuildPlan;
use zup_bundle::{
    AutoPayloadSource as PortableAutoPayloadSource, BundleWriter, CompiledPluginArtifact,
    DirectoryPayloadSource, Package, PackageError, PackagePayloadSource, PayloadError,
    PayloadReader, PayloadSource,
};
use zup_core::{
    Frontend, PLUGIN_PAYLOAD_ROOT, RelativePath, Sha256Digest, TargetTriple, hash_reader,
};

const RESOURCE_TYPE_RCDATA: usize = 10;
const RESOURCE_ID_INDEX: usize = 1;
const RESOURCE_ID_BLOB_START: usize = 2;
const WINDOWS_X64_TARGET: &str = "x86_64-pc-windows-msvc";
const WINDOWS_ARM64_TARGET: &str = "aarch64-pc-windows-msvc";
const PE_SUBSYSTEM_OFFSET: u64 = 68;
const PE_MIN_OPTIONAL_HEADER_SIZE: u64 = PE_SUBSYSTEM_OFFSET + 2;
const IMAGE_SUBSYSTEM_CUI: u16 = 3;
const IMAGE_SUBSYSTEM_GUI: u16 = 2;
const MAX_RESOURCE_SIZE: u64 = u32::MAX as u64;

/// Errors produced by the Windows package adapter.
#[derive(Debug, Error)]
pub enum BundleError {
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error("bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundle is truncated, corrupt, or unsupported")]
    Invalid,
    #[error("executable has no embedded package index")]
    MissingResource,
    #[error("package data is {size} bytes; the Windows resource limit is {limit} bytes")]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("package has {count} blobs; the Windows resource identifier limit is {limit}")]
    TooManyBlobs { count: usize, limit: usize },
    #[error("cannot allocate {size} bytes while processing executable resources")]
    ResourceAllocation { size: u64 },
    #[error("PE resource API failed: {0}")]
    ResourceApi(u32),
    #[error("PE resource APIs are available only on Windows")]
    ResourcesUnavailable,
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
        if index_info.blob_count() > u16::MAX as usize - 1 {
            return Err(BundleError::TooManyBlobs {
                count: index_info.blob_count(),
                limit: u16::MAX as usize - 1,
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

    pub fn build_plan(&self) -> Result<zup_build::BuildPlan, BundleError> {
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
            Err(_package_error) if looks_like_pe(&path) => {
                let bundle = EmbeddedBundle::open(&path)?;
                Ok(Self::Embedded(bundle.payload_source()))
            }
            Err(package_error) => Err(BundleError::Package(package_error)),
        }
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
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && &magic == b"MZ"
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
    let machine = read_pe_header(path)?.machine;
    let target = match machine {
        0x8664 => WINDOWS_X64_TARGET,
        0xaa64 => WINDOWS_ARM64_TARGET,
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
    match read_pe_header(path)?.subsystem {
        IMAGE_SUBSYSTEM_CUI => Ok(PeSubsystem::Console),
        IMAGE_SUBSYSTEM_GUI => Ok(PeSubsystem::Gui),
        _ => Err(BundleError::Invalid),
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

struct PeHeader {
    machine: u16,
    subsystem: u16,
    security_offset: u64,
}

fn read_pe_header(path: &Path) -> Result<PeHeader, BundleError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < 0x40 {
        return Err(BundleError::Invalid);
    }
    let mut dos = [0u8; 0x40];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(BundleError::Invalid);
    }
    let pe = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if pe < 0x40 || pe.checked_add(24).is_none_or(|end| end > len) {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(pe))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(BundleError::Invalid);
    }
    let mut coff = [0u8; 20];
    file.read_exact(&mut coff)?;
    let machine = u16::from_le_bytes(coff[..2].try_into().unwrap());
    let section_count = u16::from_le_bytes(coff[2..4].try_into().unwrap());
    let optional_offset = pe + 24;
    let optional_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let optional_end = optional_offset
        .checked_add(optional_len)
        .ok_or(BundleError::Invalid)?;
    if optional_len < PE_MIN_OPTIONAL_HEADER_SIZE || optional_end > len || section_count == 0 {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(optional_offset))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    let data_directory_offset = match u16::from_le_bytes(magic) {
        0x10b => 96,
        0x20b => 112,
        _ => return Err(BundleError::Invalid),
    };
    let security_offset = optional_offset
        .checked_add(data_directory_offset)
        .and_then(|offset| offset.checked_add(8 * 4))
        .ok_or(BundleError::Invalid)?;
    if security_offset
        .checked_add(8)
        .is_none_or(|end| end > optional_end)
    {
        return Err(BundleError::Invalid);
    }
    let subsystem_offset = optional_offset
        .checked_add(PE_SUBSYSTEM_OFFSET)
        .ok_or(BundleError::Invalid)?;
    file.seek(SeekFrom::Start(subsystem_offset))?;
    let mut subsystem = [0u8; 2];
    file.read_exact(&mut subsystem)?;
    let section_end = optional_end
        .checked_add(
            u64::from(section_count)
                .checked_mul(40)
                .ok_or(BundleError::Invalid)?,
        )
        .ok_or(BundleError::Invalid)?;
    if section_end > len {
        return Err(BundleError::Invalid);
    }
    Ok(PeHeader {
        machine,
        subsystem: u16::from_le_bytes(subsystem),
        security_offset,
    })
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    let header = read_pe_header(path)?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(header.security_offset))?;
    let mut certificate = [0u8; 8];
    file.read_exact(&mut certificate)?;
    if certificate != [0; 8] {
        return Err(BundleError::RuntimeAlreadySigned);
    }
    Ok(())
}

#[cfg(windows)]
fn embed_bundle_resource(
    executable: &Path,
    output: &Path,
    package_path: &Path,
) -> Result<(), BundleError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_link::link;

    type Handle = *mut core::ffi::c_void;
    type Bool = i32;
    type Dword = u32;

    link!("kernel32.dll" "system" fn BeginUpdateResourceW(filename: *const u16, delete_existing: Bool) -> Handle);
    link!("kernel32.dll" "system" fn UpdateResourceW(update: Handle, resource_type: *const u16, name: *const u16, language: u16, data: *const core::ffi::c_void, size: Dword) -> Bool);
    link!("kernel32.dll" "system" fn EndUpdateResourceW(update: Handle, discard: Bool) -> Bool);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);

    if output.exists() || output == executable {
        return Err(BundleError::Invalid);
    }
    let package = Package::open_unverified(package_path)?;
    let index = package.index_bytes()?;
    if index.len() as u64 > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size: index.len() as u64,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    if package.blob_count() > u16::MAX as usize - 1 {
        return Err(BundleError::TooManyBlobs {
            count: package.blob_count(),
            limit: u16::MAX as usize - 1,
        });
    }
    for index in 0..package.blob_count() {
        let size = package.index_info().compressed_size(index).unwrap_or(0);
        if size > MAX_RESOURCE_SIZE {
            return Err(BundleError::ResourceTooLarge {
                size,
                limit: MAX_RESOURCE_SIZE,
            });
        }
    }
    std::fs::copy(executable, output)?;
    let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    if update.is_null() {
        let _ = std::fs::remove_file(output);
        return Err(BundleError::ResourceApi(unsafe { GetLastError() }));
    }
    let apply = |id: usize, data: &[u8]| -> Result<(), u32> {
        let size = u32::try_from(data.len()).map_err(|_| 87u32)?;
        let ok = unsafe {
            UpdateResourceW(
                update,
                RESOURCE_TYPE_RCDATA as *const u16,
                id as *const u16,
                0,
                data.as_ptr().cast(),
                size,
            )
        };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(())
        }
    };
    let result = (|| {
        apply(RESOURCE_ID_INDEX, &index).map_err(BundleError::ResourceApi)?;
        for index in 0..package.blob_count() {
            let blob = package.compressed_blob(index)?;
            apply(index + RESOURCE_ID_BLOB_START, &blob).map_err(BundleError::ResourceApi)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            EndUpdateResourceW(update, 1);
        }
        let _ = std::fs::remove_file(output);
        return Err(error);
    }
    if unsafe { EndUpdateResourceW(update, 0) } == 0 {
        let error = unsafe { GetLastError() };
        let _ = std::fs::remove_file(output);
        return Err(BundleError::ResourceApi(error));
    }
    Ok(())
}

#[cfg(not(windows))]
fn embed_bundle_resource(
    _executable: &Path,
    _output: &Path,
    _package_path: &Path,
) -> Result<(), BundleError> {
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(windows)]
fn read_resource(path: &Path, resource_id: usize) -> Result<Vec<u8>, BundleError> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_link::link;

    type Handle = *mut core::ffi::c_void;
    type Dword = u32;

    link!("kernel32.dll" "system" fn LoadLibraryExW(filename: *const u16, file: Handle, flags: Dword) -> Handle);
    link!("kernel32.dll" "system" fn FindResourceW(module: Handle, name: *const u16, resource_type: *const u16) -> Handle);
    link!("kernel32.dll" "system" fn LoadResource(module: Handle, resource: Handle) -> Handle);
    link!("kernel32.dll" "system" fn SizeofResource(module: Handle, resource: Handle) -> Dword);
    link!("kernel32.dll" "system" fn LockResource(resource: Handle) -> *const core::ffi::c_void);
    link!("kernel32.dll" "system" fn FreeLibrary(module: Handle) -> i32);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 0x0000_0002) };
    if module.is_null() {
        return Err(BundleError::ResourceApi(unsafe { GetLastError() }));
    }
    let resource = unsafe {
        FindResourceW(
            module,
            resource_id as *const u16,
            RESOURCE_TYPE_RCDATA as *const u16,
        )
    };
    let result = if resource.is_null() {
        Err(BundleError::MissingResource)
    } else {
        let size = unsafe { SizeofResource(module, resource) } as usize;
        if size == 0 {
            Err(BundleError::Invalid)
        } else {
            let loaded = unsafe { LoadResource(module, resource) };
            let data = if loaded.is_null() {
                ptr::null()
            } else {
                unsafe { LockResource(loaded) }
            };
            if data.is_null() {
                Err(BundleError::ResourceApi(unsafe { GetLastError() }))
            } else {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size)
                    .map_err(|_| BundleError::ResourceAllocation { size: size as u64 })?;
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data.cast::<u8>(), size)
                });
                Ok(bytes)
            }
        }
    };
    unsafe {
        FreeLibrary(module);
    }
    result
}

#[cfg(not(windows))]
fn read_resource(_path: &Path, _resource_id: usize) -> Result<Vec<u8>, BundleError> {
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn pe_bytes(subsystem: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x170];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
        bytes
    }

    #[test]
    fn pe_frontend_reads_cui_and_gui_subsystems() {
        let directory = tempfile::tempdir().unwrap();
        let cui = directory.path().join("cui.exe");
        let gui = directory.path().join("gui.exe");
        fs::write(&cui, pe_bytes(3)).unwrap();
        fs::write(&gui, pe_bytes(2)).unwrap();
        assert_eq!(read_pe_frontend(&cui).unwrap(), Frontend::Console);
        assert_eq!(read_pe_frontend(&gui).unwrap(), Frontend::Gui);
    }

    #[test]
    fn pe_frontend_rejects_unknown_subsystems() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime.exe");
        fs::write(&runtime, pe_bytes(9)).unwrap();
        assert!(matches!(
            read_pe_frontend(&runtime),
            Err(BundleError::Invalid)
        ));
    }

    #[test]
    fn pe_frontend_validation_rejects_mismatches() {
        let directory = tempfile::tempdir().unwrap();
        let cui = directory.path().join("cui.exe");
        fs::write(&cui, pe_bytes(3)).unwrap();
        assert!(validate_pe_frontend(&cui, Frontend::Console).is_ok());
        assert!(matches!(
            validate_pe_frontend(&cui, Frontend::Gui),
            Err(BundleError::FrontendMismatch {
                expected: Frontend::Gui,
                found: Frontend::Console,
            })
        ));
    }
}
