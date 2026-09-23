//! Versioned, content-addressed bundle objects and PE overlay location.

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
const FOOTER: &[u8; 8] = b"ZUPLOC\0\0";
const SCHEMA: u32 = 1;
const HEADER_LEN: u64 = 60;
const FOOTER_LEN: u64 = 64;
const MAX_METADATA: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundle metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle is truncated, corrupt, unsupported, or has unsafe offsets")]
    Invalid,
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
        for (digest, bytes) in contents {
            let encoded = zstd::stream::encode_all(Cursor::new(&bytes), 9)?;
            let offset = compressed.len() as u64;
            blobs.push(BlobIndex {
                digest,
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
            return Err(BundleError::Invalid);
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
        for (digest, spool) in &objects {
            blobs.push(BlobIndex {
                digest: *digest,
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
            return Err(BundleError::Invalid);
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

/// Validated package located in a standalone bundle or a PE overlay.
pub struct EmbeddedBundle {
    path: PathBuf,
    package_offset: u64,
    package_len: u64,
    metadata: Metadata,
}

impl EmbeddedBundle {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let len = file.metadata()?.len();
        let (start, package_len) = locate_package(&mut file, len)?;
        let metadata = parse_metadata(&mut file, start, package_len)?;
        let bundle = Self {
            path,
            package_offset: start,
            package_len,
            metadata,
        };
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
            package_offset: self.package_offset,
            package_len: self.package_len,
            blobs: self.metadata.blobs.clone(),
            entries: self.metadata.plan.entries.clone(),
        }
    }
    fn verify_all(&self) -> Result<(), BundleError> {
        for entry in &self.metadata.plan.entries {
            let blob = self
                .metadata
                .blobs
                .iter()
                .find(|b| b.digest == entry.blob)
                .ok_or(BundleError::Invalid)?;
            let mut decoder = open_blob(&self.path, self.package_offset, self.package_len, blob)?;
            let (size, hash) = hash_reader(&mut decoder)?;
            if size != blob.size
                || hash != blob.digest
                || size != entry.size
                || hash != entry.sha256
            {
                return Err(BundleError::Payload(entry.path.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct BundlePayloadSource {
    path: PathBuf,
    package_offset: u64,
    package_len: u64,
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
        let decoder = open_blob(&self.path, self.package_offset, self.package_len, blob).map_err(
            |source| crate::PayloadError::Read {
                path: path.to_string(),
                source: std::io::Error::other(source.to_string()),
            },
        )?;
        Ok(Box::new(decoder))
    }
}

fn parse_metadata(file: &mut File, start: u64, package_len: u64) -> Result<Metadata, BundleError> {
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
        || meta_len > MAX_METADATA
        || HEADER_LEN
            .checked_add(meta_len)
            .is_none_or(|n| n > package_len)
    {
        return Err(BundleError::Invalid);
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
    for blob in &metadata.blobs {
        let end = blob
            .offset
            .checked_add(blob.compressed_size)
            .ok_or(BundleError::Invalid)?;
        if blob.offset != previous
            || end > data_len
            || by_digest.insert(blob.digest, blob.size).is_some()
        {
            return Err(BundleError::Invalid);
        }
        previous = end;
    }
    if previous != data_len {
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
    path: &Path,
    package_offset: u64,
    package_len: u64,
    blob: &BlobIndex,
) -> Result<
    zstd::stream::read::Decoder<'static, std::io::BufReader<std::io::Take<File>>>,
    BundleError,
> {
    let mut file = File::open(path)?;
    let data_start = package_offset
        .checked_add(HEADER_LEN)
        .ok_or(BundleError::Invalid)?;
    // Metadata length is recovered from the fixed header.
    file.seek(SeekFrom::Start(package_offset + 20))?;
    let mut b = [0; 8];
    file.read_exact(&mut b)?;
    let meta_len = u64::from_le_bytes(b);
    let pos = data_start
        .checked_add(meta_len)
        .and_then(|n| n.checked_add(blob.offset))
        .ok_or(BundleError::Invalid)?;
    let end = blob
        .offset
        .checked_add(blob.compressed_size)
        .ok_or(BundleError::Invalid)?;
    if end > package_len.saturating_sub(HEADER_LEN + meta_len) {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(pos))?;
    Ok(zstd::stream::read::Decoder::new(
        file.take(blob.compressed_size),
    )?)
}

fn locate_package(file: &mut File, len: u64) -> Result<(u64, u64), BundleError> {
    if len >= FOOTER_LEN
        && let Ok(found) = footer_at(file, len - FOOTER_LEN, len - FOOTER_LEN)
    {
        return Ok(found);
    }
    let cert = pe_certificate_range(file, len)?;
    if let Some((offset, size)) = cert {
        if offset >= FOOTER_LEN {
            return footer_at(file, offset - FOOTER_LEN, offset - FOOTER_LEN);
        }
        let _ = size;
    }
    Err(BundleError::Invalid)
}
fn footer_at(file: &mut File, at: u64, package_end: u64) -> Result<(u64, u64), BundleError> {
    file.seek(SeekFrom::Start(at))?;
    let mut b = [0; 64];
    file.read_exact(&mut b)?;
    if &b[..8] != FOOTER
        || u32::from_le_bytes(b[8..12].try_into().unwrap()) != 1
        || b[12..16] != [0; 4]
    {
        return Err(BundleError::Invalid);
    }
    let start = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let size = u64::from_le_bytes(b[24..32].try_into().unwrap());
    if start.checked_add(size) != Some(at) || at != package_end || size < HEADER_LEN {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut h = [0; 8];
    file.read_exact(&mut h)?;
    if &h != MAGIC {
        return Err(BundleError::Invalid);
    }
    let mut hash = Sha256::new();
    file.seek(SeekFrom::Start(start))?;
    let mut take = file.take(size);
    std::io::copy(&mut take, &mut HashWriter(&mut hash))?;
    if hash.finalize().as_slice() != &b[32..64] {
        return Err(BundleError::Invalid);
    }
    Ok((start, size))
}
struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn pe_certificate_range(file: &mut File, len: u64) -> Result<Option<(u64, u64)>, BundleError> {
    if len < 0x40 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0x3c))?;
    let mut p = [0; 4];
    file.read_exact(&mut p)?;
    let pe = u32::from_le_bytes(p) as u64;
    if pe.checked_add(24).is_none_or(|n| n > len) {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(pe))?;
    let mut sig = [0; 4];
    file.read_exact(&mut sig)?;
    if &sig != b"PE\0\0" {
        return Ok(None);
    }
    let mut coff = [0; 20];
    file.read_exact(&mut coff)?;
    let opt_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let opt = pe + 24;
    if opt.checked_add(opt_len).is_none_or(|n| n > len) || opt_len < 2 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(opt))?;
    let mut magic = [0; 2];
    file.read_exact(&mut magic)?;
    let dir_base = match u16::from_le_bytes(magic) {
        0x10b => 96u64,
        0x20b => 112u64,
        _ => return Ok(None),
    };
    let security = opt
        .checked_add(dir_base + 8 * 4)
        .ok_or(BundleError::Invalid)?;
    if security + 8 > opt + opt_len {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(security))?;
    let mut entry = [0; 8];
    file.read_exact(&mut entry)?;
    let offset = u32::from_le_bytes(entry[..4].try_into().unwrap()) as u64;
    let size = u32::from_le_bytes(entry[4..].try_into().unwrap()) as u64;
    if offset == 0 && size == 0 {
        return Ok(None);
    }
    if offset.checked_add(size) != Some(len) || size == 0 {
        return Ok(None);
    }
    Ok(Some((offset, size)))
}

/// Append a package plus an authenticated locator footer to a PE executable.
/// The footer sits immediately before any later Authenticode certificate table.
pub fn append_bundle_to_executable(
    executable: &Path,
    output: &Path,
    package: &[u8],
) -> Result<u64, BundleError> {
    if output.exists() {
        return Err(BundleError::Invalid);
    }
    let temp = output.with_extension(format!("tmp-{}", std::process::id()));
    let mut src = File::open(executable)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    std::io::copy(&mut src, &mut dst)?;
    let start = dst.stream_position()?;
    dst.write_all(package)?;
    let mut footer = Vec::with_capacity(64);
    footer.extend_from_slice(FOOTER);
    footer.extend_from_slice(&1u32.to_le_bytes());
    footer.extend_from_slice(&0u32.to_le_bytes());
    footer.extend_from_slice(&start.to_le_bytes());
    footer.extend_from_slice(&(package.len() as u64).to_le_bytes());
    footer.extend_from_slice(&Sha256::digest(package));
    dst.write_all(&footer)?;
    dst.sync_all()?;
    let size = start + package.len() as u64 + FOOTER_LEN;
    drop(dst);
    std::fs::rename(&temp, output)?;
    Ok(size)
}

/// Build a self-contained executable without holding the package contents in
/// memory. Both payload compression and the final PE overlay are streamed.
pub fn build_self_contained_executable(
    executable: &Path,
    output: &Path,
    plan: &BuildPlan,
) -> Result<(u64, u64), BundleError> {
    validate_unsigned_pe(executable)?;
    let temp = tempfile::tempdir()?;
    let package = temp.path().join("installer.zupbundle");
    let package_size = BundleWriter::write_file(plan, &package)?;
    let exe_size = append_bundle_file_to_executable(executable, output, &package)?;
    Ok((exe_size, package_size + FOOTER_LEN))
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < 0x40 {
        return Err(BundleError::Invalid);
    }
    let mut dos = [0; 2];
    file.read_exact(&mut dos)?;
    if &dos != b"MZ" {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(0x3c))?;
    let mut pe_offset = [0; 4];
    file.read_exact(&mut pe_offset)?;
    let pe_offset = u32::from_le_bytes(pe_offset) as u64;
    if pe_offset.checked_add(4).is_none_or(|end| end > len) {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(pe_offset))?;
    let mut signature = [0; 4];
    file.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(BundleError::Invalid);
    }
    if pe_certificate_range(&mut file, len)?.is_some() {
        return Err(BundleError::Invalid);
    }
    Ok(())
}

pub fn append_bundle_file_to_executable(
    executable: &Path,
    output: &Path,
    package: &Path,
) -> Result<u64, BundleError> {
    if output.exists() {
        return Err(BundleError::Invalid);
    }
    let temp = output.with_extension(format!("tmp-{}", std::process::id()));
    let mut src = File::open(executable)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    std::io::copy(&mut src, &mut dst)?;
    let start = dst.stream_position()?;
    let package_size = std::fs::metadata(package)?.len();
    let mut hash = Sha256::new();
    {
        let mut tee = HashingWriter {
            output: &mut dst,
            hash: &mut hash,
        };
        std::io::copy(&mut File::open(package)?, &mut tee)?;
    }
    let mut footer = Vec::with_capacity(FOOTER_LEN as usize);
    footer.extend_from_slice(FOOTER);
    footer.extend_from_slice(&1u32.to_le_bytes());
    footer.extend_from_slice(&0u32.to_le_bytes());
    footer.extend_from_slice(&start.to_le_bytes());
    footer.extend_from_slice(&package_size.to_le_bytes());
    footer.extend_from_slice(&hash.finalize());
    dst.write_all(&footer)?;
    dst.sync_all()?;
    let size = start
        .checked_add(package_size)
        .and_then(|n| n.checked_add(FOOTER_LEN))
        .ok_or(BundleError::Invalid)?;
    drop(dst);
    std::fs::rename(&temp, output)?;
    Ok(size)
}

struct HashingWriter<'a, W> {
    output: &'a mut W,
    hash: &'a mut Sha256,
}
impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.hash.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
