use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use zup_acquire::{
    CatalogEntry, ContentCatalog, DocumentRef, RELEASE_SCHEMA, ReleaseDescriptor, ReleaseVariant,
    WebLayout, blob_path, check_channel, check_segment,
};
use zup_core::Sha256Digest;

use crate::compose::ArtifactGraph;
use crate::format::ArtifactError;
use crate::format::Descriptor;
use crate::store::SegmentReader;
use crate::table::BlobTable;
use crate::variant::VariantManifest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTree {
    pub root: PathBuf,
    pub blob_count: u64,
    pub blob_bytes: u64,
    pub variant_count: u64,
    pub release_digest: Sha256Digest,
    pub tuf_targets: Vec<String>,
}

impl WebTree {
    fn empty() -> Self {
        Self {
            root: PathBuf::new(),
            blob_count: 0,
            blob_bytes: 0,
            variant_count: 0,
            release_digest: Sha256Digest::from_bytes([0u8; 32]),
            tuf_targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebExport {
    pub channel: String,
}

impl WebExport {
    pub fn new(channel: &str) -> Result<Self, ArtifactError> {
        check_channel(channel).map_err(|_| ArtifactError::Invalid)?;
        Ok(Self {
            channel: channel.to_owned(),
        })
    }
}

pub fn export_web_tree(
    graph: &ArtifactGraph,
    channel: &WebExport,
    destination: &Path,
) -> Result<WebTree, ArtifactError> {
    export_web_tree_with(graph, channel, destination, &[])
}

/// updater never needs it.
pub fn export_web_tree_with(
    graph: &ArtifactGraph,
    channel: &WebExport,
    destination: &Path,
    downloads: &[ReleaseFile],
) -> Result<WebTree, ArtifactError> {
    std::fs::create_dir_all(destination).map_err(ArtifactError::from)?;

    let table = graph.table();
    let segments = graph.segments()?;
    let mut tree = WebTree {
        root: destination.to_path_buf(),
        ..WebTree::empty()
    };
    let mut catalog_entries: Vec<CatalogEntry> = Vec::new();

    write_blobs(
        table,
        &segments,
        graph,
        destination,
        &mut tree,
        &mut catalog_entries,
    )?;
    let (release, tuf_targets) = write_release(
        graph,
        channel,
        destination,
        downloads,
        &mut tree,
        catalog_entries,
    )?;

    tree.release_digest = release.release_digest;
    tree.tuf_targets = tuf_targets;
    Ok(tree)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseFile {
    pub path: String,
    pub digest: Sha256Digest,
    pub size: u64,
    pub kind: zup_acquire::ReleaseDownloadKind,
    pub variant: Option<String>,
}

fn write_blobs(
    table: &BlobTable,
    segments: &crate::store::FileSegments,
    graph: &ArtifactGraph,
    destination: &Path,
    tree: &mut WebTree,
    catalog_entries: &mut Vec<CatalogEntry>,
) -> Result<(), ArtifactError> {
    for entry in table.entries() {
        let relative = blob_path(&entry.digest).to_string();
        let path = join_relative(destination, &relative)?;
        if path.is_file()
            && std::fs::metadata(&path)
                .map(|metadata| metadata.len() == entry.compressed_size)
                .unwrap_or(false)
        {
            tree.blob_count += 1;
            tree.blob_bytes = tree.blob_bytes.saturating_add(entry.compressed_size);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(ArtifactError::from)?;
            }
            let bytes = segments.read_range(entry.segment, entry.offset, entry.compressed_size)?;
            write_atomic(&path, &bytes)?;
            tree.blob_count += 1;
            tree.blob_bytes = tree.blob_bytes.saturating_add(entry.compressed_size);
        }
        catalog_entries.push(CatalogEntry::compressed(
            entry.digest,
            entry.compressed_size,
            entry.size,
        ));
    }

    for (id, bytes) in graph.runtime_bytes()? {
        let runtime = graph
            .index()
            .variant(&id)
            .and_then(|variant| variant.runtime)
            .ok_or(ArtifactError::Invalid)?;
        write_native_image(destination, tree, &runtime, &bytes, catalog_entries)?;
    }

    for (id, bytes) in graph.preset_bytes() {
        let preset = graph
            .index()
            .variant(&id)
            .and_then(|variant| variant.preset)
            .ok_or(ArtifactError::Invalid)?;
        write_native_image(destination, tree, &preset, &bytes, catalog_entries)?;
    }
    Ok(())
}

fn write_native_image(
    destination: &Path,
    tree: &mut WebTree,
    image: &Descriptor,
    bytes: &[u8],
    catalog_entries: &mut Vec<CatalogEntry>,
) -> Result<(), ArtifactError> {
    let path = join_relative(destination, &blob_path(&image.digest).to_string())?;
    if !path.is_file() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ArtifactError::from)?;
        }
        write_atomic(&path, bytes)?;
        tree.blob_count += 1;
        tree.blob_bytes = tree.blob_bytes.saturating_add(bytes.len() as u64);
    }
    catalog_entries.push(CatalogEntry::stored(image.digest, bytes.len() as u64));
    Ok(())
}

fn write_release(
    graph: &ArtifactGraph,
    channel: &WebExport,
    destination: &Path,
    downloads: &[ReleaseFile],
    tree: &mut WebTree,
    catalog_entries: Vec<CatalogEntry>,
) -> Result<(ReleaseDescriptor, Vec<String>), ArtifactError> {
    let catalog = ContentCatalog::new(catalog_entries).map_err(|_| ArtifactError::Invalid)?;
    let catalog_bytes = catalog.encode().map_err(|_| ArtifactError::Invalid)?;
    let catalog_path = WebLayout::catalog(&channel.channel).map_err(|_| ArtifactError::Invalid)?;
    write_atomic(
        &join_relative(destination, &catalog_path.to_string())?,
        &catalog_bytes,
    )?;

    let mut variants = Vec::new();
    let mut tuf_targets = Vec::new();
    for manifest in graph.manifests() {
        let variant = write_variant(
            graph,
            &channel.channel,
            &manifest.id,
            &manifest.bytes,
            destination,
        )?;
        tree.variant_count += 1;
        tuf_targets.push(relative(&catalog_path));
        tuf_targets.push(relative(
            &WebLayout::variant_manifest(&channel.channel, &variant.id)
                .map_err(|_| ArtifactError::Invalid)?,
        ));
        variants.push(variant);
    }
    variants.sort_by(|left, right| left.id.cmp(&right.id));

    let index = graph.index();
    let mut release = ReleaseDescriptor {
        schema: RELEASE_SCHEMA,
        app_id: index.artifact.application.id.clone(),
        channel: channel.channel.clone(),
        version: index.artifact.application.version.to_string(),
        release_digest: Sha256Digest::from_bytes([0u8; 32]),
        catalog: DocumentRef::of(
            zup_acquire::ContentDescriptor::of_document(
                zup_acquire::ContentKind::Catalog,
                &catalog_bytes,
                zup_acquire::ContentPriority::Normal,
            )
            .map_err(|_| ArtifactError::Invalid)?
            .digest,
            catalog_bytes.len() as u64,
        ),
        variants,
        downloads: downloads
            .iter()
            .map(|file| zup_acquire::ReleaseDownload {
                kind: file.kind,
                path: file.path.clone(),
                descriptor: DocumentRef::of(file.digest, file.size),
                variant: file.variant.clone(),
            })
            .collect(),
    };
    release.release_digest = release
        .computed_digest()
        .map_err(|_| ArtifactError::Invalid)?;
    release.validate().map_err(|_| ArtifactError::Invalid)?;

    let release_bytes = release.encode().map_err(|_| ArtifactError::Invalid)?;
    let release_path = WebLayout::release(&channel.channel).map_err(|_| ArtifactError::Invalid)?;
    write_atomic(
        &join_relative(destination, &release_path.to_string())?,
        &release_bytes,
    )?;
    tuf_targets.push(relative(&release_path));

    let versioned = WebLayout::release_version(&channel.channel, &release.version)
        .map_err(|_| ArtifactError::Invalid)?;
    write_atomic(
        &join_relative(destination, &versioned.to_string())?,
        &release_bytes,
    )?;
    tuf_targets.push(relative(&versioned));

    write_tuf_input(
        destination,
        &release_bytes,
        &catalog_bytes,
        graph,
        &channel.channel,
        &release.version,
    )?;
    Ok((release, tuf_targets))
}

fn write_variant(
    _graph: &ArtifactGraph,
    channel: &str,
    id: &str,
    manifest_bytes: &[u8],
    destination: &Path,
) -> Result<ReleaseVariant, ArtifactError> {
    check_segment(id).map_err(|_| ArtifactError::Invalid)?;
    let manifest: VariantManifest = VariantManifest::parse(manifest_bytes)?;
    let path = WebLayout::variant_manifest(channel, id).map_err(|_| ArtifactError::Invalid)?;
    write_atomic(
        &join_relative(destination, &path.to_string())?,
        manifest_bytes,
    )?;

    let native = |descriptor: &Option<Descriptor>| {
        descriptor
            .as_ref()
            .map(|descriptor| DocumentRef::of(descriptor.digest, descriptor.size))
    };
    let runtime = native(&manifest.runtime);
    let preset = native(&manifest.preset);
    let target = manifest.plan.installer.target.clone();
    Ok(ReleaseVariant {
        id: id.to_owned(),
        platform: target.to_string(),
        target: target.clone(),
        frontend: manifest.plan.installer.frontend.as_str().to_owned(),
        manifest: DocumentRef::of(
            zup_acquire::ContentDescriptor::of_document(
                zup_acquire::ContentKind::Metadata,
                manifest_bytes,
                zup_acquire::ContentPriority::Normal,
            )
            .map_err(|_| ArtifactError::Invalid)?
            .digest,
            manifest_bytes.len() as u64,
        ),
        runtime,
        preset,
        content: manifest.content_digests(),
        requirements: Default::default(),
        logical_size: manifest.logical_size,
    })
}

fn write_tuf_input(
    destination: &Path,
    release_bytes: &[u8],
    catalog_bytes: &[u8],
    graph: &ArtifactGraph,
    channel: &str,
    version: &str,
) -> Result<(), ArtifactError> {
    let root = destination.join("tuf-input");
    let mut documents: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let release_relative = WebLayout::release(channel).map_err(|_| ArtifactError::Invalid)?;
    documents.insert(relative(&release_relative), release_bytes.to_vec());
    let versioned =
        WebLayout::release_version(channel, version).map_err(|_| ArtifactError::Invalid)?;
    documents.insert(relative(&versioned), release_bytes.to_vec());
    let catalog_relative = WebLayout::catalog(channel).map_err(|_| ArtifactError::Invalid)?;
    documents.insert(relative(&catalog_relative), catalog_bytes.to_vec());
    for manifest in graph.manifests() {
        let path = WebLayout::variant_manifest(channel, &manifest.id)
            .map_err(|_| ArtifactError::Invalid)?;
        documents.insert(relative(&path), manifest.bytes.clone());
    }
    for (path, bytes) in documents {
        write_atomic(&join_relative(&root, &path)?, &bytes)?;
    }
    Ok(())
}

fn relative(path: &zup_acquire::RelativeContentPath) -> String {
    path.to_string()
}

fn join_relative(root: &Path, relative: &str) -> Result<PathBuf, ArtifactError> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ArtifactError::Invalid);
        }
        path.push(segment);
    }
    if !path.starts_with(root) {
        return Err(ArtifactError::Invalid);
    }
    Ok(path)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ArtifactError::from)?;
    }
    let temporary = path.with_extension("zup-partial");
    {
        let mut file = std::fs::File::create(&temporary).map_err(ArtifactError::from)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(ArtifactError::from)?;
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            std::fs::rename(&temporary, path).map_err(|_| ArtifactError::Io(error))
        }
    }
}
