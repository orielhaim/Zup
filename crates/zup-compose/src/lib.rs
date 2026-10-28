//! Portable installer composition and verified content extraction.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use zup_binary::{BinaryFormat, Executable};
use zup_bundle::Package;
use zup_core::{Frontend, Sha256Digest, Target};
use zup_pe::{
    MAX_RESOURCE_ID, MAX_RESOURCE_SIZE, RESOURCE_ID_BLOB_START, RESOURCE_ID_INDEX,
    RESOURCE_ID_PRESET, ResourceDocument,
};

pub const CARRIER_MAGIC: &[u8; 17] = b"ZUP-LINUX-CARRIER";

pub const CARRIER_VERSION: u32 = 2;

pub const CARRIER_FOOTER_LEN: u64 = 121;

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("installer I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Inspect(#[from] zup_binary::InspectError),
    #[error(transparent)]
    Target(#[from] zup_binary::TargetRefusal),
    #[error(transparent)]
    Frontend(#[from] zup_binary::FrontendRefusal),
    #[error(transparent)]
    Pe(#[from] zup_pe::PeError),
    #[error(transparent)]
    Package(#[from] zup_bundle::PackageError),
    #[error("package data is {size} bytes; the Windows resource limit is {limit} bytes")]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("package has {count} blobs; the Windows resource identifier limit is {limit}")]
    TooManyBlobs { count: usize, limit: usize },
    #[error("cannot allocate {size} bytes while processing executable resources")]
    ResourceAllocation { size: u64 },
    #[error("required package resource {id} is missing or has an invalid size")]
    InvalidResource { id: usize },
    #[error("runtime already has a certificate table; embed before signing")]
    RuntimeAlreadySigned,
    #[error("package installs `{expected}` and `{found}` was requested")]
    TargetMismatch { expected: Target, found: Target },
    #[error("`{path}` is a {format} image and installs `{expected}`")]
    FormatMismatch {
        path: String,
        format: String,
        expected: String,
    },
    #[error("`{path}` is {size} bytes, too short to be a zup installer")]
    TooShort { path: String, size: u64 },
    #[error("`{path}` carries no zup installer content")]
    NoContent { path: String },
    #[error("`{path}` declares installer format {found}, expected {expected}")]
    UnsupportedVersion {
        path: String,
        found: u32,
        expected: u32,
    },
    #[error("`{path}` declares unsupported carrier flags {flags:#x}")]
    UnsupportedFlags { path: String, flags: u32 },
    #[error("`{path}` declares its package at {offset}..{end}, outside the {size}-byte file")]
    ContentOutsideFile {
        path: String,
        offset: u64,
        end: u64,
        size: u64,
    },
    #[error("`{path}` declares a {length}-byte package, which is not plausible")]
    ImplausibleLength { path: String, length: u64 },
    #[error(
        "`{path}` carries a package that hashes to {found}, not the {expected} its footer declares"
    )]
    PackageDigestMismatch {
        path: String,
        expected: Sha256Digest,
        found: Sha256Digest,
    },
    #[error(
        "`{path}` carries a preset that hashes to {found}, not the {expected} its footer declares"
    )]
    PresetDigestMismatch {
        path: String,
        expected: Sha256Digest,
        found: Sha256Digest,
    },
    #[error("invalid compressed preset: {0}")]
    Preset(String),
    #[error("refusing to compose an installer over its own runtime template")]
    OutputIsRuntime,
}

impl ComposeError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeReport {
    pub output_size: u64,
    pub package_size: u64,
    pub preset_size: u64,
}

#[derive(Debug)]
pub struct OpenedInstaller {
    pub package: Package,
    pub preset: Option<Vec<u8>>,
}

impl OpenedInstaller {
    pub fn target(&self) -> &Target {
        &self.package.plan().installer.target
    }

    pub fn frontend(&self) -> Frontend {
        self.package.plan().installer.frontend
    }
}

pub fn compose(
    target: &Target,
    runtime: &Path,
    output: &Path,
    package: &[u8],
    preset: Option<&[u8]>,
) -> Result<ComposeReport, ComposeError> {
    if output == runtime {
        return Err(ComposeError::OutputIsRuntime);
    }
    let parsed = Package::from_bytes_unverified(package.to_vec())?;
    if parsed.plan().installer.target != *target {
        return Err(ComposeError::TargetMismatch {
            expected: parsed.plan().installer.target,
            found: *target,
        });
    }
    match target.operating_system() {
        zup_core::TargetOperatingSystem::Linux => compose_linux(runtime, output, package, preset),
        _ => compose_windows(runtime, output, package, preset),
    }
}

pub fn open(path: &Path) -> Result<OpenedInstaller, ComposeError> {
    let image = Executable::read(path)?;
    match image.format() {
        BinaryFormat::Pe => open_windows(path),
        BinaryFormat::Elf => open_linux(path),
        format => Err(ComposeError::FormatMismatch {
            path: path.display().to_string(),
            format: format.to_string(),
            expected: "a Windows or Linux installer".to_owned(),
        }),
    }
}

pub fn compose_windows(
    runtime: &Path,
    output: &Path,
    package: &[u8],
    preset: Option<&[u8]>,
) -> Result<ComposeReport, ComposeError> {
    if output == runtime {
        return Err(ComposeError::OutputIsRuntime);
    }
    let parsed = Package::from_bytes_unverified(package.to_vec())?;
    let installer = &parsed.plan().installer;
    let image = Executable::read(runtime)?;
    if image.format() != BinaryFormat::Pe {
        return Err(ComposeError::FormatMismatch {
            path: runtime.display().to_string(),
            format: image.format().to_string(),
            expected: installer.target.to_string(),
        });
    }
    image.refuse_target(&installer.target)?;
    image.refuse_frontend(installer.frontend)?;
    if zup_pe::is_signed(runtime)? {
        return Err(ComposeError::RuntimeAlreadySigned);
    }
    let mut documents = package_documents(&parsed)?;
    let preset = compress_preset(preset)?;
    let preset_size = preset.as_ref().map(|bytes| bytes.len() as u64).unwrap_or(0);
    if let Some(bytes) = preset {
        if bytes.len() as u64 > MAX_RESOURCE_SIZE {
            return Err(ComposeError::ResourceTooLarge {
                size: bytes.len() as u64,
                limit: MAX_RESOURCE_SIZE,
            });
        }
        documents.push(ResourceDocument {
            id: RESOURCE_ID_PRESET,
            bytes,
        });
    }
    zup_pe::check_documents(&documents, RESOURCE_ID_INDEX)?;
    let template = std::fs::read(runtime).map_err(|error| ComposeError::io(runtime, error))?;
    let composed = zup_pe::resources::update_rcdata(&template, &documents)?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| ComposeError::io(parent, error))?;
    zup_platform::publish(output, &composed).map_err(|error| ComposeError::Io {
        path: output.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })?;
    Ok(ComposeReport {
        output_size: composed.len() as u64,
        package_size: package.len() as u64,
        preset_size,
    })
}

pub fn apply_windows_icon(
    output: &Path,
    images: &[Vec<u8>],
    group: &[u8],
) -> Result<(), ComposeError> {
    if images.is_empty() || group.is_empty() {
        return Ok(());
    }
    if images.len() > MAX_RESOURCE_ID {
        return Err(ComposeError::TooManyBlobs {
            count: images.len(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let image = std::fs::read(output).map_err(|error| ComposeError::io(output, error))?;
    let stamped = zup_pe::resources::update_icons(&image, images, group)?;
    zup_platform::publish(output, &stamped).map_err(|error| ComposeError::Io {
        path: output.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })?;
    Ok(())
}

fn package_documents(package: &Package) -> Result<Vec<ResourceDocument>, ComposeError> {
    let index = package.index_bytes()?;
    if index.len() as u64 > MAX_RESOURCE_SIZE {
        return Err(ComposeError::ResourceTooLarge {
            size: index.len() as u64,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    if package.blob_count() > MAX_RESOURCE_ID {
        return Err(ComposeError::TooManyBlobs {
            count: package.blob_count(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let mut documents = Vec::with_capacity(package.blob_count() + 1);
    documents.push(ResourceDocument {
        id: RESOURCE_ID_INDEX,
        bytes: index,
    });
    for index in 0..package.blob_count() {
        if package.index_info().compressed_size(index).unwrap_or(0) > MAX_RESOURCE_SIZE {
            return Err(ComposeError::ResourceTooLarge {
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

fn read_windows_resource(image: &[u8], id: usize) -> Result<Option<Vec<u8>>, ComposeError> {
    if id > MAX_RESOURCE_ID {
        return Ok(None);
    }
    Ok(zup_pe::resources::read_resource(
        image,
        zup_pe::resources::RESOURCE_TYPE_RCDATA,
        id as u32,
    )?)
}

pub fn open_windows(path: &Path) -> Result<OpenedInstaller, ComposeError> {
    let image = std::fs::read(path).map_err(|error| ComposeError::io(path, error))?;
    let Some(index) = read_windows_resource(&image, RESOURCE_ID_INDEX)? else {
        return Err(ComposeError::NoContent {
            path: path.display().to_string(),
        });
    };
    let index_info = Package::parse_index(&index)?;
    if index_info.blob_count() > MAX_RESOURCE_ID {
        return Err(ComposeError::TooManyBlobs {
            count: index_info.blob_count(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let mut blobs = Vec::new();
    blobs
        .try_reserve_exact(index_info.blob_count())
        .map_err(|_| ComposeError::ResourceAllocation {
            size: index_info.blob_count() as u64,
        })?;
    let mut total_size = index.len() as u64;
    for index in 0..index_info.blob_count() {
        let expected = index_info
            .compressed_size(index)
            .ok_or(ComposeError::NoContent {
                path: path.display().to_string(),
            })?;
        let blob = read_windows_resource(&image, index + RESOURCE_ID_BLOB_START)?.ok_or(
            ComposeError::InvalidResource {
                id: index + RESOURCE_ID_BLOB_START,
            },
        )?;
        if blob.len() as u64 != expected {
            return Err(ComposeError::InvalidResource {
                id: index + RESOURCE_ID_BLOB_START,
            });
        }
        total_size = total_size
            .checked_add(blob.len() as u64)
            .ok_or(ComposeError::NoContent {
                path: path.display().to_string(),
            })?;
        blobs.push(blob);
    }
    let total_size = usize::try_from(total_size).map_err(|_| ComposeError::NoContent {
        path: path.display().to_string(),
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_size)
        .map_err(|_| ComposeError::ResourceAllocation {
            size: total_size as u64,
        })?;
    bytes.extend_from_slice(&index);
    for blob in blobs {
        bytes.extend_from_slice(&blob);
    }
    let package = Package::from_bytes(bytes)?;
    let preset = read_windows_resource(&image, RESOURCE_ID_PRESET)?
        .map(|bytes| decompress_preset(&bytes))
        .transpose()?;
    Ok(OpenedInstaller { package, preset })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CarrierFooter {
    version: u32,
    flags: u32,
    package_offset: u64,
    package_length: u64,
    package_digest: Sha256Digest,
    preset_length: u64,
    preset_digest: Sha256Digest,
}

impl CarrierFooter {
    fn to_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(CARRIER_FOOTER_LEN as usize);
        bytes.extend_from_slice(CARRIER_MAGIC);
        bytes.extend_from_slice(&self.version.to_le_bytes());
        bytes.extend_from_slice(&self.flags.to_le_bytes());
        bytes.extend_from_slice(&self.package_offset.to_le_bytes());
        bytes.extend_from_slice(&self.package_length.to_le_bytes());
        bytes.extend_from_slice(self.package_digest.as_bytes());
        bytes.extend_from_slice(&self.preset_length.to_le_bytes());
        bytes.extend_from_slice(self.preset_digest.as_bytes());
        bytes.extend_from_slice(&CARRIER_FOOTER_LEN.to_le_bytes());
        bytes
    }
}

fn read_footer(
    file: &mut std::fs::File,
    path: &Path,
    file_size: u64,
) -> Result<CarrierFooter, ComposeError> {
    if file_size < CARRIER_FOOTER_LEN {
        return Err(ComposeError::TooShort {
            path: path.display().to_string(),
            size: file_size,
        });
    }
    file.seek(SeekFrom::Start(file_size - CARRIER_FOOTER_LEN))
        .map_err(|error| ComposeError::io(path, error))?;
    let mut tail = [0u8; CARRIER_FOOTER_LEN as usize];
    file.read_exact(&mut tail)
        .map_err(|error| ComposeError::io(path, error))?;
    if tail[..CARRIER_MAGIC.len()] != *CARRIER_MAGIC {
        return Err(ComposeError::NoContent {
            path: path.display().to_string(),
        });
    }
    let version = u32::from_le_bytes(tail[17..21].try_into().expect("four bytes"));
    if version != CARRIER_VERSION {
        return Err(ComposeError::UnsupportedVersion {
            path: path.display().to_string(),
            found: version,
            expected: CARRIER_VERSION,
        });
    }
    let declared_len = u64::from_le_bytes(tail[tail.len() - 8..].try_into().expect("eight bytes"));
    if declared_len != CARRIER_FOOTER_LEN {
        return Err(ComposeError::UnsupportedVersion {
            path: path.display().to_string(),
            found: declared_len as u32,
            expected: CARRIER_VERSION,
        });
    }
    let footer = CarrierFooter {
        version,
        flags: u32::from_le_bytes(tail[21..25].try_into().expect("four bytes")),
        package_offset: u64::from_le_bytes(tail[25..33].try_into().expect("eight bytes")),
        package_length: u64::from_le_bytes(tail[33..41].try_into().expect("eight bytes")),
        package_digest: Sha256Digest::from_bytes(
            tail[41..73].try_into().expect("thirty-two bytes"),
        ),
        preset_length: u64::from_le_bytes(tail[73..81].try_into().expect("eight bytes")),
        preset_digest: Sha256Digest::from_bytes(
            tail[81..113].try_into().expect("thirty-two bytes"),
        ),
    };
    if footer.flags != 0 {
        return Err(ComposeError::UnsupportedFlags {
            path: path.display().to_string(),
            flags: footer.flags,
        });
    }
    check_ranges(&footer, path, file_size)?;
    Ok(footer)
}

fn check_ranges(footer: &CarrierFooter, path: &Path, file_size: u64) -> Result<(), ComposeError> {
    let package_end = footer
        .package_offset
        .checked_add(footer.package_length)
        .ok_or(ComposeError::ContentOutsideFile {
            path: path.display().to_string(),
            offset: footer.package_offset,
            end: u64::MAX,
            size: file_size,
        })?;
    let preset_end =
        package_end
            .checked_add(footer.preset_length)
            .ok_or(ComposeError::ContentOutsideFile {
                path: path.display().to_string(),
                offset: package_end,
                end: u64::MAX,
                size: file_size,
            })?;
    if preset_end.checked_add(CARRIER_FOOTER_LEN) != Some(file_size) {
        return Err(ComposeError::ContentOutsideFile {
            path: path.display().to_string(),
            offset: footer.package_offset,
            end: preset_end,
            size: file_size,
        });
    }
    Ok(())
}

fn read_range(
    file: &mut std::fs::File,
    path: &Path,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, ComposeError> {
    let length = usize::try_from(length).map_err(|_| ComposeError::ImplausibleLength {
        path: path.display().to_string(),
        length,
    })?;
    let mut bytes = vec![0u8; length];
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| ComposeError::io(path, error))?;
    file.read_exact(&mut bytes)
        .map_err(|error| ComposeError::io(path, error))?;
    Ok(bytes)
}

pub fn compose_linux(
    runtime: &Path,
    output: &Path,
    package: &[u8],
    preset: Option<&[u8]>,
) -> Result<ComposeReport, ComposeError> {
    if output == runtime {
        return Err(ComposeError::OutputIsRuntime);
    }
    if package.is_empty() {
        return Err(ComposeError::ImplausibleLength {
            path: output.display().to_string(),
            length: 0,
        });
    }
    let image = Executable::read(runtime)?;
    if image.format() != BinaryFormat::Elf {
        return Err(ComposeError::FormatMismatch {
            path: runtime.display().to_string(),
            format: image.format().to_string(),
            expected: "a Linux runtime".to_owned(),
        });
    }
    let preset = compress_preset(preset)?.unwrap_or_default();
    let mut template =
        std::fs::File::open(runtime).map_err(|error| ComposeError::io(runtime, error))?;
    let runtime_len = template
        .metadata()
        .map_err(|error| ComposeError::io(runtime, error))?
        .len();
    let footer = CarrierFooter {
        version: CARRIER_VERSION,
        flags: 0,
        package_offset: runtime_len,
        package_length: package.len() as u64,
        package_digest: zup_core::hash_bytes(package),
        preset_length: preset.len() as u64,
        preset_digest: zup_core::hash_bytes(&preset),
    };
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| ComposeError::io(parent, error))?;
    let temporary = staging_path(output);
    let composed = (|| -> Result<(), std::io::Error> {
        let mut out = std::fs::File::create(&temporary)?;
        std::io::copy(&mut template, &mut out)?;
        out.write_all(package)?;
        out.write_all(&preset)?;
        out.write_all(&footer.to_bytes())?;
        out.sync_all()
    })();
    if let Err(error) = composed {
        let _ = std::fs::remove_file(&temporary);
        return Err(ComposeError::io(output, error));
    }
    let bytes = std::fs::read(&temporary).map_err(|error| ComposeError::io(&temporary, error))?;
    zup_platform::publish(output, &bytes).map_err(|error| ComposeError::Io {
        path: output.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })?;
    let _ = std::fs::remove_file(&temporary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| ComposeError::io(output, error))?;
    }
    Ok(ComposeReport {
        output_size: bytes.len() as u64,
        package_size: package.len() as u64,
        preset_size: preset.len() as u64,
    })
}

fn staging_path(output: &Path) -> PathBuf {
    output.with_extension("zup-partial")
}

pub fn open_linux(path: &Path) -> Result<OpenedInstaller, ComposeError> {
    let mut file = std::fs::File::open(path).map_err(|error| ComposeError::io(path, error))?;
    let file_size = file
        .metadata()
        .map_err(|error| ComposeError::io(path, error))?
        .len();
    let footer = read_footer(&mut file, path, file_size)?;
    let package_bytes = read_range(
        &mut file,
        path,
        footer.package_offset,
        footer.package_length,
    )?;
    if zup_core::hash_bytes(&package_bytes) != footer.package_digest {
        return Err(ComposeError::PackageDigestMismatch {
            path: path.display().to_string(),
            expected: footer.package_digest,
            found: zup_core::hash_bytes(&package_bytes),
        });
    }
    let package = Package::from_bytes(package_bytes)?;
    let preset = if footer.preset_length > 0 {
        let preset_bytes = read_range(
            &mut file,
            path,
            footer.package_offset + footer.package_length,
            footer.preset_length,
        )?;
        if zup_core::hash_bytes(&preset_bytes) != footer.preset_digest {
            return Err(ComposeError::PresetDigestMismatch {
                path: path.display().to_string(),
                expected: footer.preset_digest,
                found: zup_core::hash_bytes(&preset_bytes),
            });
        }
        Some(decompress_preset(&preset_bytes)?)
    } else {
        None
    };
    let expected = package.plan().installer.target;
    let image = Executable::read(path).map_err(ComposeError::from)?;
    image.refuse_target(&expected)?;
    if image.format() != BinaryFormat::Elf
        || expected.operating_system() != zup_core::TargetOperatingSystem::Linux
    {
        return Err(ComposeError::FormatMismatch {
            path: path.display().to_string(),
            format: image.format().to_string(),
            expected: expected.to_string(),
        });
    }
    Ok(OpenedInstaller { package, preset })
}

fn compress_preset(bytes: Option<&[u8]>) -> Result<Option<Vec<u8>>, ComposeError> {
    bytes
        .map(|bytes| {
            if bytes.is_empty() || bytes.len() as u64 > zup_core::MAX_PRESET_BINARY_BYTES {
                return Err(ComposeError::Preset(
                    "executable size exceeds its bounds".into(),
                ));
            }
            zstd::bulk::compress(bytes, 9).map_err(|error| ComposeError::Preset(error.to_string()))
        })
        .transpose()
}

fn decompress_preset(bytes: &[u8]) -> Result<Vec<u8>, ComposeError> {
    let size = zstd::zstd_safe::get_frame_content_size(bytes)
        .map_err(|error| ComposeError::Preset(error.to_string()))?
        .filter(|size| *size > 0 && *size <= zup_core::MAX_PRESET_BINARY_BYTES)
        .ok_or_else(|| ComposeError::Preset("executable size exceeds its bounds".into()))?;
    zstd::bulk::decompress(bytes, size as usize)
        .map_err(|error| ComposeError::Preset(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn compressed_presets_reject_malformed_and_oversized_frames() {
        assert!(decompress_preset(b"not a compressed executable").is_err());
        let mut header = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
        header.extend_from_slice(&(zup_core::MAX_PRESET_BINARY_BYTES + 1).to_le_bytes());
        assert!(decompress_preset(&header).is_err());
    }

    fn runtime_bytes(marker: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes.extend_from_slice(marker.as_bytes());
        bytes.resize(512, 0);
        bytes
    }

    fn runtime_file(directory: &Path, marker: &str) -> PathBuf {
        let path = directory.join("runtime");
        std::fs::write(&path, runtime_bytes(marker)).expect("write runtime");
        path
    }

    fn linux_package() -> (Vec<u8>, Target) {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("dist")).expect("dist");
        std::fs::write(root.path().join("dist/a.bin"), b"payload").expect("write");
        let manifest = r#"
schema = 1
[app]
id = "com.example.compose"
name = "Compose Test"
version = "1.0.0"
[build]
[build.targets.default]
target = "linux-x64"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/ComposeTest"
[[files]]
source = "**/*"
destination = "${install}"
"#;
        let parsed = zup_manifest::parse(manifest).expect("parse");
        let installer = zup_manifest::parse_and_compile(manifest, "default").expect("compile");
        let config = zup_manifest::select_targets(
            &parsed,
            &["default"],
            &zup_core::TargetOverrides::default(),
        )
        .expect("select")
        .into_iter()
        .next()
        .expect("one target");
        let mut build = zup_build::materialize(
            &root.path().join("zup.toml"),
            &parsed,
            vec![(config, installer)],
            zup_build::Writes::None,
        )
        .expect("materialize");
        let plan = build.targets.pop().expect("one plan");
        let target = plan.installer.target;
        let bytes = zup_bundle::BundleWriter::encode(&plan, &[]).expect("encode");
        (bytes, target)
    }

    #[test]
    fn a_linux_carrier_round_trips_package_and_preset() {
        let (package, target) = linux_package();
        assert_eq!(target.as_str(), "linux-x64");
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        let preset = vec![b'x'; 64 * 1024];
        let report = compose(&target, &runtime, &output, &package, Some(&preset)).expect("compose");
        assert_eq!(report.package_size, package.len() as u64);
        assert!(report.preset_size < preset.len() as u64);
        assert_eq!(
            std::fs::metadata(&output).expect("stat").len(),
            report.output_size,
        );
        let opened = open(&output).expect("open");
        assert_eq!(opened.preset.as_deref(), Some(preset.as_slice()));
        assert_eq!(opened.target(), &target);
    }

    #[test]
    fn a_linux_carrier_without_a_preset_opens_with_none() {
        let (package, target) = linux_package();
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&target, &runtime, &output, &package, None).expect("compose");
        let opened = open(&output).expect("open");
        assert_eq!(opened.preset, None);
    }

    #[test]
    fn a_package_for_another_target_is_refused_before_any_byte_is_written() {
        let (package, _) = linux_package();
        let windows = Target::parse("windows-x64").expect("valid");
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        let error = compose(&windows, &runtime, &output, &package, None)
            .expect_err("a Linux package is not a Windows installer");
        assert!(matches!(error, ComposeError::TargetMismatch { .. }));
        assert!(!output.exists());
    }

    #[test]
    fn a_linux_carrier_never_modifies_its_template() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let before = std::fs::read(&runtime).expect("read");
        compose_linux(
            &runtime,
            &directory.path().join("out"),
            b"pkg",
            Some(b"preset"),
        )
        .expect("compose");
        assert_eq!(std::fs::read(&runtime).expect("read"), before);
    }

    #[test]
    fn edited_linux_package_bytes_are_refused() {
        let (package, _) = linux_package();
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose_linux(&runtime, &output, &package, None).expect("compose");
        let mut tampered = std::fs::read(&output).expect("read");
        tampered[runtime_bytes("zup-installer").len()] = b'X';
        let path = directory.path().join("tampered");
        std::fs::write(&path, &tampered).expect("write");
        assert!(matches!(
            open_linux(&path),
            Err(ComposeError::PackageDigestMismatch { .. })
        ));
    }

    #[test]
    fn edited_linux_preset_bytes_are_refused() {
        let (package, _) = linux_package();
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose_linux(&runtime, &output, &package, Some(b"preset")).expect("compose");
        let mut tampered = std::fs::read(&output).expect("read");
        tampered[runtime_bytes("zup-installer").len() + package.len()] = b'X';
        let path = directory.path().join("tampered");
        std::fs::write(&path, &tampered).expect("write");
        assert!(matches!(
            open_linux(&path),
            Err(ComposeError::PresetDigestMismatch { .. })
        ));
    }

    #[rstest]
    #[case::shorter_than_a_footer(vec![0u8; 8])]
    #[case::long_enough_but_not_a_carrier(vec![0u8; 200])]
    fn malformed_linux_content_is_refused_without_panicking(#[case] bytes: Vec<u8>) {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("carrier");
        std::fs::write(&path, &bytes).expect("write");
        assert!(open_linux(&path).is_err());
    }

    #[test]
    fn composing_over_the_runtime_itself_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        assert!(matches!(
            compose_linux(&runtime, &runtime, b"pkg", None),
            Err(ComposeError::OutputIsRuntime)
        ));
    }

    #[test]
    fn a_non_elf_runtime_is_refused_for_linux() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = directory.path().join("runtime");
        std::fs::write(&runtime, b"MZ not an elf image").expect("write");
        let output = directory.path().join("out");
        assert!(compose_linux(&runtime, &output, b"pkg", None).is_err());
    }

    #[test]
    fn a_non_pe_runtime_is_refused_for_windows() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = directory.path().join("runtime");
        std::fs::write(&runtime, b"\x7fELF not a pe image").expect("write");
        let output = directory.path().join("out.exe");
        assert!(compose_windows(&runtime, &output, b"not a package", None).is_err());
    }
}
