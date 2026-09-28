//! Export a composed artifact as a static web tree.
//!
//! This is the developer-facing half of the online story: from one build, the
//! bytes a static host serves and the documents a TUF repository signs. Nothing
//! here needs a server that understands components, targets, or installation -
//! the origin is a file tree, because every object in it is named by its
//! identity.
//!
//! ```text
//! <out>/blobs/sha256/<ab>/<hex>            one compressed blob
//! <out>/releases/<channel>.json            the release descriptor
//! <out>/releases/<channel>/catalog.json    digest-to-size catalog
//! <out>/releases/<channel>/variants/*.json one variant manifest
//! <out>/tuf-input/…                        the same documents, for tuftool
//! ```
//!
//! Two trees come out because two different consumers come out. The web tree is
//! what a client fetches. The TUF input tree is what `tuftool --add-targets`
//! consumes, because the release graph has to be *signed* before a client will
//! believe it, and signing stays outside zup.
//!
//! The blob bytes are copied straight out of the composed store. Nothing is
//! recompressed and nothing is rehashed, so a blob that was verified at
//! composition time is the blob an origin serves, and the digest in its name is
//! the digest of the content behind it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use zup_acquire::{
    CatalogEntry, ContentCatalog, DocumentRef, RELEASE_SCHEMA, ReleaseDescriptor, ReleaseVariant,
    WebLayout, blob_path, check_channel, check_segment,
};
use zup_core::Sha256Digest;

use crate::compose::ArtifactGraph;
use crate::error::ArtifactError;
use crate::store::SegmentReader;
use crate::table::BlobTable;
use crate::variant::VariantManifest;

/// What an export produced, so a build can report it and a test can assert it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTree {
    /// Where the tree was written.
    pub root: PathBuf,
    /// Blobs written, and the wire bytes they took.
    pub blob_count: u64,
    pub blob_bytes: u64,
    /// Variant manifests written.
    pub variant_count: u64,
    /// The release descriptor's own digest, which is the release's identity.
    pub release_digest: Sha256Digest,
    /// Documents placed under the TUF input tree, for `tuftool --add-targets`.
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

/// Which channel the exported release answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebExport {
    pub channel: String,
}

impl WebExport {
    /// A named channel.
    pub fn new(channel: &str) -> Result<Self, ArtifactError> {
        check_channel(channel).map_err(|_| ArtifactError::Invalid)?;
        Ok(Self {
            channel: channel.to_owned(),
        })
    }
}

/// Write the whole web tree for `graph` into `destination`.
///
/// The directory is created if it does not exist. An existing tree is not
/// cleared: a blob that is already there with the right name is already the
/// right bytes, and re-exporting an unchanged release should not rewrite a
/// gigabyte.
pub fn export_web_tree(
    graph: &ArtifactGraph,
    channel: &WebExport,
    destination: &Path,
) -> Result<WebTree, ArtifactError> {
    export_web_tree_with(graph, channel, destination, &[])
}

/// Write the web tree, recording the finished installer files as release
/// downloads.
///
/// The offline installer is a claim in the graph rather than a separate
/// package: an enterprise or disconnected user still gets one file, and the
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

/// A finished installer a human can download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseFile {
    pub path: String,
    pub digest: Sha256Digest,
    pub size: u64,
    pub kind: zup_acquire::ReleaseDownloadKind,
    pub variant: Option<String>,
}

/// Write the blobs the graph carries, including each variant's native runtime.
///
/// A runtime is not payload. Composition keeps it as a descriptor and the
/// offline artifact carries it as its own region, so it is not in the composed
/// store - but an online machine has to fetch it, which means it has to be an
/// addressable object too. It is written as its raw bytes: compressing
/// executable code buys nothing, and the client verifies the digest of the image
/// it is about to run.
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
            // The bytes are copied exactly as composed. Recompressing here would
            // produce a different wire form for the same content, which is legal
            // and pointless: the client decompresses and hashes either way, and a
            // stable byte layout is what lets an origin cache a blob forever.
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
        let variant = graph
            .manifests()
            .iter()
            .find(|manifest| manifest.id == id)
            .ok_or(ArtifactError::Invalid)?;
        let runtime = graph
            .index()
            .variant(&variant.id)
            .and_then(|variant| variant.runtime)
            .ok_or(ArtifactError::Invalid)?;
        let path = join_relative(destination, &blob_path(&runtime.digest).to_string())?;
        if !path.is_file() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(ArtifactError::from)?;
            }
            write_atomic(&path, &bytes)?;
            tree.blob_count += 1;
            tree.blob_bytes = tree.blob_bytes.saturating_add(bytes.len() as u64);
        }
        catalog_entries.push(CatalogEntry::stored(runtime.digest, bytes.len() as u64));
    }
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

    // The same body, addressed by version. A channel document moves; a version
    // document does not, and that is what lets a version-pinned thin installer
    // exist at all: it authenticates this name, which no later publication can
    // change. The bytes are identical, so the release digest is the same and a
    // pinned and a channel client that land on one version can prove they would
    // install the same thing.
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

    // A runtime is content like any other. Naming it in the release is what lets
    // a thin bootstrapper verify the executable before it runs it, which is the
    // one thing a bootstrapper is not allowed to skip.
    let runtime = manifest
        .runtime
        .as_ref()
        .map(|descriptor| DocumentRef::of(descriptor.digest, descriptor.size));
    let target = manifest.plan.installer.target.clone();
    Ok(ReleaseVariant {
        id: id.to_owned(),
        // The platform and frontend are claims a client selects on, so they come
        // from the manifest the plan carries rather than from a filename or an
        // ordering convention.
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
        content: manifest.content_digests(),
        requirements: Default::default(),
        logical_size: manifest.logical_size,
    })
}

/// Mirror the authenticated documents into a TUF input tree.
///
/// Signing stays outside zup: this writes the files `tuftool --add-targets`
/// reads, and nothing more. There is no key handling, no signature format, and
/// no second application-level signing scheme.
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

/// Join a `/`-separated relative path onto a root, refusing anything that leaves
/// it. Every path here is computed by this crate, and the check is here so a
/// future edit cannot turn a computed name into a traversal.
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

/// Write a file through a temporary sibling, so an interrupted export never
/// leaves a half-written document that a client would try to parse.
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
