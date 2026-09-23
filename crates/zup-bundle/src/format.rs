//! Versioned, content-addressed bundle objects stored in an Authenticode-hashed
//! PE RCDATA resource.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_build::BuildPlan;
use zup_core::{
    ComponentId, Condition, Installer, RelativePath, Sha256Digest, Template, hash_reader,
};

const MAGIC: &[u8; 8] = b"ZUPBNDL\0";
const SCHEMA: u32 = 2;
const HEADER_LEN: u64 = 60;
const MAX_METADATA: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_RESOURCE_SIZE: u64 = u32::MAX as u64;
const RESOURCE_TYPE_RCDATA: usize = 10;
const RESOURCE_ID_BUNDLE: usize = 1;

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundle metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle is truncated, corrupt, unsupported, or has unsafe offsets")]
    Invalid,
    #[error(
        "installer package is {size} bytes; the Windows RCDATA resource limit is {limit} bytes"
    )]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("bundle index is {size} bytes; the metadata limit is {limit} bytes")]
    MetadataTooLarge { size: u64, limit: u64 },
    #[error(
        "installer has {count} unique payload blobs; numeric RCDATA identifiers allow at most {limit}"
    )]
    TooManyBlobs { count: usize, limit: usize },
    #[error("cannot allocate {size} bytes while processing the installer resource")]
    ResourceAllocation { size: u64 },
    #[error("PE resource API failed: {0}")]
    ResourceApi(u32),
    #[error("PE resource APIs are available only on Windows")]
    ResourcesUnavailable,
    #[error(
        "runtime already has an Authenticode certificate table; embed resources before signing"
    )]
    RuntimeAlreadySigned,
    #[error("payload verification failed for {0}")]
    Payload(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableBuildPlan {
    pub installer: Installer,
    pub entries: Vec<PayloadEntry>,
    pub total_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadEntry {
    pub path: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub blob: Sha256Digest,
    pub component: Option<ComponentId>,
    pub condition: Option<Condition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobIndex {
    digest: Sha256Digest,
    resource_id: u16,
    offset: u64,
    compressed_size: u64,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: u32,
    required_features: u64,
    plan: PortableBuildPlan,
    blobs: Vec<BlobIndex>,
}

#[derive(Debug)]
pub struct BundleWriter;

impl BundleWriter {
    /// Produce deterministic package bytes. Blobs are Zstandard-compressed once
    /// per unique SHA-256 identity; all offsets are relative to the data region.
    pub fn encode(plan: &BuildPlan) -> Result<Vec<u8>, BundleError> {
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        let mut contents = BTreeMap::<Sha256Digest, Vec<u8>>::new();
        for file in &plan.files {
            let bytes = std::fs::read(&file.source)?;
            if bytes.len() as u64 != file.size
                || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != file.sha256
            {
                return Err(BundleError::Payload(file.source.display().to_string()));
            }
            contents.entry(file.sha256).or_insert(bytes);
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size: file.size,
                sha256: file.sha256,
                blob: file.sha256,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        entries.sort_by(|a, b| {
            a.destination
                .to_string()
                .cmp(&b.destination.to_string())
                .then(a.path.cmp(&b.path))
        });
        let total_size = entries.iter().try_fold(0u64, |sum, e| {
            sum.checked_add(e.size).ok_or(BundleError::Invalid)
        })?;
        let mut compressed = Vec::new();
        let mut blobs = Vec::new();
        if contents.len() > u16::MAX as usize - 1 {
            return Err(BundleError::TooManyBlobs {
                count: contents.len(),
                limit: u16::MAX as usize - 1,
            });
        }
        for (index, (digest, bytes)) in contents.into_iter().enumerate() {
            let encoded = zstd::stream::encode_all(Cursor::new(&bytes), 9)?;
            let offset = compressed.len() as u64;
            blobs.push(BlobIndex {
                digest,
                resource_id: u16::try_from(index + 2).map_err(|_| BundleError::Invalid)?,
                offset,
                compressed_size: encoded.len() as u64,
                size: bytes.len() as u64,
            });
            compressed.extend_from_slice(&encoded);
        }
        let plan = PortableBuildPlan {
            installer,
            entries,
            total_size,
        };
        // Blob offsets are independent of metadata length.
        let metadata = Metadata {
            schema: SCHEMA,
            required_features: 0,
            plan,
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(BundleError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let meta_hash = Sha256::digest(&meta);
        let mut out = Vec::with_capacity(HEADER_LEN as usize + meta.len() + compressed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&SCHEMA.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // required features
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&meta_hash);
        out.extend_from_slice(&meta);
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Stream unique payload files through Zstandard into a spool directory,
    /// then write the index followed by compressed objects to `output`.
    pub fn write_file(plan: &BuildPlan, output: &Path) -> Result<u64, BundleError> {
        struct Spool {
            path: PathBuf,
            size: u64,
            compressed_size: u64,
        }
        let spool_dir = tempfile::tempdir()?;
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        let mut objects = BTreeMap::<Sha256Digest, Spool>::new();
        for file in &plan.files {
            if let std::collections::btree_map::Entry::Vacant(entry) = objects.entry(file.sha256) {
                let mut input = File::open(&file.source)?;
                let spool_path = spool_dir.path().join(file.sha256.to_hex());
                let output_file = File::create(&spool_path)?;
                let mut encoder = zstd::stream::write::Encoder::new(output_file, 9)?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    size = size.checked_add(read as u64).ok_or(BundleError::Invalid)?;
                    hasher.update(&buffer[..read]);
                    encoder.write_all(&buffer[..read])?;
                }
                let output_file = encoder.finish()?;
                if size != file.size || Sha256Digest::from_hasher(hasher) != file.sha256 {
                    return Err(BundleError::Payload(file.source.display().to_string()));
                }
                let compressed_size = output_file.metadata()?.len();
                entry.insert(Spool {
                    path: spool_path,
                    size,
                    compressed_size,
                });
            } else {
                let (size, digest) = hash_reader(File::open(&file.source)?)?;
                if size != file.size || digest != file.sha256 {
                    return Err(BundleError::Payload(file.source.display().to_string()));
                }
            }
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size: file.size,
                sha256: file.sha256,
                blob: file.sha256,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        entries.sort_by(|a, b| {
            a.destination
                .to_string()
                .cmp(&b.destination.to_string())
                .then(a.path.cmp(&b.path))
        });
        let total_size = entries.iter().try_fold(0u64, |sum, e| {
            sum.checked_add(e.size).ok_or(BundleError::Invalid)
        })?;
        let mut offset = 0u64;
        let mut blobs = Vec::with_capacity(objects.len());
        if objects.len() > u16::MAX as usize - 1 {
            return Err(BundleError::TooManyBlobs {
                count: objects.len(),
                limit: u16::MAX as usize - 1,
            });
        }
        for (index, (digest, spool)) in objects.iter().enumerate() {
            blobs.push(BlobIndex {
                digest: *digest,
                resource_id: u16::try_from(index + 2).map_err(|_| BundleError::Invalid)?,
                offset,
                compressed_size: spool.compressed_size,
                size: spool.size,
            });
            offset = offset
                .checked_add(spool.compressed_size)
                .ok_or(BundleError::Invalid)?;
        }
        let metadata = Metadata {
            schema: SCHEMA,
            required_features: 0,
            plan: PortableBuildPlan {
                installer,
                entries,
                total_size,
            },
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(BundleError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;
        out.write_all(MAGIC)?;
        out.write_all(&SCHEMA.to_le_bytes())?;
        out.write_all(&0u64.to_le_bytes())?;
        out.write_all(&(meta.len() as u64).to_le_bytes())?;
        out.write_all(&Sha256::digest(&meta))?;
        out.write_all(&meta)?;
        for spool in objects.values() {
            std::io::copy(&mut File::open(&spool.path)?, &mut out)?;
        }
        out.sync_all()?;
        Ok(out.metadata()?.len())
    }
}

/// Validated content index from the executable's RCDATA resources.
pub struct EmbeddedBundle {
    path: PathBuf,
    metadata: Metadata,
}

impl EmbeddedBundle {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = path.as_ref().to_path_buf();
        let temporary = tempfile::tempdir()?;
        let package_path = temporary.path().join("bundle.index");
        extract_bundle_resource(&path, &package_path)?;
        let mut file = File::open(&package_path)?;
        let len = file.metadata()?.len();
        let metadata = parse_metadata(&mut file, 0, len, true)?;
        let bundle = Self { path, metadata };
        bundle.verify_all()?;
        Ok(bundle)
    }

    pub fn plan(&self) -> &PortableBuildPlan {
        &self.metadata.plan
    }
    pub fn build_plan(&self) -> Result<BuildPlan, BundleError> {
        let files = self
            .metadata
            .plan
            .entries
            .iter()
            .map(|entry| {
                Ok(zup_build::ResolvedFile {
                    source: PathBuf::new(),
                    source_relative: entry.path.clone(),
                    destination: entry.destination.clone(),
                    size: entry.size,
                    sha256: entry
                        .sha256
                        .to_string()
                        .parse()
                        .map_err(|_| BundleError::Invalid)?,
                    component: entry.component.clone(),
                    condition: entry.condition.clone(),
                })
            })
            .collect::<Result<Vec<_>, BundleError>>()?;
        Ok(BuildPlan {
            installer: self.metadata.plan.installer.clone(),
            files,
            total_size: self.metadata.plan.total_size,
        })
    }
    pub fn payload_source(&self) -> BundlePayloadSource {
        BundlePayloadSource {
            path: self.path.clone(),
            blobs: self.metadata.blobs.clone(),
            entries: self.metadata.plan.entries.clone(),
        }
    }
    fn verify_all(&self) -> Result<(), BundleError> {
        for blob in &self.metadata.blobs {
            let mut decoder = open_blob(&self.path, blob)?;
            let (size, hash) = hash_reader(&mut decoder)?;
            if size != blob.size || hash != blob.digest {
                return Err(BundleError::Payload(blob.digest.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct BundlePayloadSource {
    path: PathBuf,
    blobs: Vec<BlobIndex>,
    entries: Vec<PayloadEntry>,
}

impl crate::PayloadSource for BundlePayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<crate::PayloadReader, crate::PayloadError> {
        if path.as_str() == "__zup_maintenance__.exe" {
            let file = File::open(&self.path).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            let (size, digest) = hash_reader(file).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            if size != expected_size {
                return Err(crate::PayloadError::SizeMismatch {
                    path: path.to_string(),
                    expected: expected_size,
                    found: size,
                });
            }
            if digest != *expected_sha256 {
                return Err(crate::PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            let file = File::open(&self.path).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            return Ok(Box::new(file));
        }
        let entry = self
            .entries
            .iter()
            .find(|e| &e.path == path && &e.sha256 == expected_sha256 && e.size == expected_size)
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let blob = self
            .blobs
            .iter()
            .find(|b| b.digest == entry.blob)
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let decoder = open_blob(&self.path, blob).map_err(|source| crate::PayloadError::Read {
            path: path.to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;
        Ok(Box::new(decoder))
    }
}

fn parse_metadata(
    file: &mut File,
    start: u64,
    package_len: u64,
    resource_index: bool,
) -> Result<Metadata, BundleError> {
    if package_len < HEADER_LEN {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut header = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC || u32::from_le_bytes(header[8..12].try_into().unwrap()) != SCHEMA {
        return Err(BundleError::Invalid);
    }
    let features = u64::from_le_bytes(header[12..20].try_into().unwrap());
    let meta_len = u64::from_le_bytes(header[20..28].try_into().unwrap());
    if features != 0
        || HEADER_LEN.checked_add(meta_len).is_none_or(|n| {
            if resource_index {
                n != package_len
            } else {
                n > package_len
            }
        })
    {
        return Err(BundleError::Invalid);
    }
    if meta_len > MAX_METADATA {
        return Err(BundleError::MetadataTooLarge {
            size: meta_len,
            limit: MAX_METADATA,
        });
    }
    let meta_size = usize::try_from(meta_len).map_err(|_| BundleError::Invalid)?;
    let mut bytes = vec![0; meta_size];
    file.read_exact(&mut bytes)?;
    if Sha256::digest(&bytes).as_slice() != &header[28..60] {
        return Err(BundleError::Invalid);
    }
    let metadata: Metadata = serde_json::from_slice(&bytes)?;
    if metadata.schema != SCHEMA
        || metadata.required_features != 0
        || metadata.plan.entries.len() > MAX_ENTRIES
    {
        return Err(BundleError::Invalid);
    }
    let computed_total = metadata.plan.entries.iter().try_fold(0u64, |sum, e| {
        sum.checked_add(e.size).ok_or(BundleError::Invalid)
    })?;
    if computed_total != metadata.plan.total_size {
        return Err(BundleError::Invalid);
    }
    if metadata.plan.entries.windows(2).any(|w| {
        w[0].destination
            .to_string()
            .cmp(&w[1].destination.to_string())
            .then(w[0].path.cmp(&w[1].path))
            .is_gt()
    }) || metadata
        .blobs
        .windows(2)
        .any(|w| w[0].digest >= w[1].digest)
    {
        return Err(BundleError::Invalid);
    }
    let data_len = package_len
        .checked_sub(HEADER_LEN + meta_len)
        .ok_or(BundleError::Invalid)?;
    let mut previous = 0u64;
    let mut by_digest = BTreeMap::new();
    let mut resource_ids = std::collections::BTreeSet::new();
    for blob in &metadata.blobs {
        let end = blob
            .offset
            .checked_add(blob.compressed_size)
            .ok_or(BundleError::Invalid)?;
        if blob.offset != previous
            || (!resource_index && end > data_len)
            || blob.resource_id < 2
            || !resource_ids.insert(blob.resource_id)
            || by_digest.insert(blob.digest, blob.size).is_some()
        {
            return Err(BundleError::Invalid);
        }
        previous = end;
    }
    if !resource_index && previous != data_len {
        return Err(BundleError::Invalid);
    }
    for entry in &metadata.plan.entries {
        if entry.blob != entry.sha256 || by_digest.get(&entry.blob) != Some(&entry.size) {
            return Err(BundleError::Invalid);
        }
    }
    Ok(metadata)
}

fn open_blob(
    executable: &Path,
    blob: &BlobIndex,
) -> Result<
    zstd::stream::read::Decoder<'static, std::io::BufReader<std::io::Cursor<Vec<u8>>>>,
    BundleError,
> {
    let bytes = read_resource(executable, blob.resource_id as usize)?;
    if bytes.len() as u64 != blob.compressed_size {
        return Err(BundleError::Invalid);
    }
    Ok(zstd::stream::read::Decoder::new(std::io::Cursor::new(
        bytes,
    ))?)
}
/// Build the final installer artifact. Authenticode signing must happen after
/// this returns so the signature covers the package resource.
pub fn build_self_contained_executable(
    executable: &Path,
    output: &Path,
    plan: &BuildPlan,
) -> Result<(u64, u64), BundleError> {
    validate_unsigned_pe(executable)?;
    let temporary = tempfile::tempdir()?;
    let package = temporary.path().join("installer.zupbundle");
    let package_size = BundleWriter::write_file(plan, &package)?;
    embed_bundle_file(executable, output, &package)?;
    Ok((std::fs::metadata(output)?.len(), package_size))
}

/// Embed a prebuilt bundle file as RCDATA resource 1. This is also used by
/// installer integration tests to exercise the exact native resource path.
pub fn embed_bundle_file(
    executable: &Path,
    output: &Path,
    package: &Path,
) -> Result<(), BundleError> {
    validate_unsigned_pe(executable)?;
    embed_bundle_resource(executable, output, package)
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < 0x40 {
        return Err(BundleError::Invalid);
    }
    let mut dos = [0u8; 2];
    file.read_exact(&mut dos)?;
    if &dos != b"MZ" {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(0x3c))?;
    let mut offset = [0u8; 4];
    file.read_exact(&mut offset)?;
    let pe = u32::from_le_bytes(offset) as u64;
    if pe.checked_add(24).is_none_or(|end| end > len) {
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
    let optional_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let optional = pe + 24;
    if optional
        .checked_add(optional_len)
        .is_none_or(|end| end > len)
        || optional_len < 2
    {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(optional))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    let data_directory_offset = match u16::from_le_bytes(magic) {
        0x10b => 96,
        0x20b => 112,
        _ => return Err(BundleError::Invalid),
    };
    let security = optional + data_directory_offset + 8 * 4;
    if security + 8 > optional + optional_len {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(security))?;
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
    package: &Path,
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
    let mut package_file = File::open(package)?;
    let package_size = package_file.metadata()?.len();
    let mut header = [0u8; HEADER_LEN as usize];
    package_file.read_exact(&mut header)?;
    let metadata_size = u64::from_le_bytes(header[20..28].try_into().unwrap());
    let index_size = HEADER_LEN
        .checked_add(metadata_size)
        .ok_or(BundleError::Invalid)?;
    if metadata_size > MAX_METADATA {
        return Err(BundleError::MetadataTooLarge {
            size: metadata_size,
            limit: MAX_METADATA,
        });
    }
    if index_size > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size: index_size,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    let metadata = parse_metadata(&mut package_file, 0, package_size, false)?;
    if metadata.blobs.len() > u16::MAX as usize - 1 {
        return Err(BundleError::TooManyBlobs {
            count: metadata.blobs.len(),
            limit: u16::MAX as usize - 1,
        });
    }
    for blob in &metadata.blobs {
        if blob.compressed_size > MAX_RESOURCE_SIZE {
            return Err(BundleError::ResourceTooLarge {
                size: blob.compressed_size,
                limit: MAX_RESOURCE_SIZE,
            });
        }
    }
    let index = read_package_range(&mut package_file, 0, index_size)?;
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
        apply(RESOURCE_ID_BUNDLE, &index).map_err(BundleError::ResourceApi)?;
        let data_start = index_size;
        for blob in &metadata.blobs {
            let start = data_start
                .checked_add(blob.offset)
                .ok_or(BundleError::Invalid)?;
            let bytes = read_package_range(&mut package_file, start, blob.compressed_size)?;
            apply(blob.resource_id as usize, &bytes).map_err(BundleError::ResourceApi)?;
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

fn read_package_range(file: &mut File, offset: u64, size: u64) -> Result<Vec<u8>, BundleError> {
    let requested_size = size;
    let size = usize::try_from(requested_size).map_err(|_| BundleError::ResourceAllocation {
        size: requested_size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| BundleError::ResourceAllocation {
            size: requested_size,
        })?;
    bytes.resize(size, 0);
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
#[cfg(not(windows))]
fn embed_bundle_resource(_: &Path, _: &Path, package: &Path) -> Result<(), BundleError> {
    let size = std::fs::metadata(package)?.len();
    if size > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(windows)]
fn read_resource(executable: &Path, resource_id: usize) -> Result<Vec<u8>, BundleError> {
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
    let wide: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
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
        Err(BundleError::Invalid)
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

#[cfg(windows)]
fn extract_bundle_resource(executable: &Path, output: &Path) -> Result<(), BundleError> {
    std::fs::write(output, read_resource(executable, RESOURCE_ID_BUNDLE)?)?;
    Ok(())
}
#[cfg(not(windows))]
fn extract_bundle_resource(_: &Path, _: &Path) -> Result<(), BundleError> {
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(not(windows))]
fn read_resource(_: &Path, _: usize) -> Result<Vec<u8>, BundleError> {
    Err(BundleError::ResourcesUnavailable)
}
