//! Transport packages: one variant's content, one asset.
//!
//! # Why a build produces them
//!
//! A project that hosts its release on GitHub needs its content to travel as
//! something GitHub can hold. Not one asset per content object — that is over
//! the per-release asset limit, unreadable in the release UI, and a
//! provider-specific copy of a data model the acquisition engine already owns —
//! and not an installer, because a package is not something anyone runs.
//!
//! So a staged release produces one package per variant, and the package is a
//! *transport container* over the same content graph:
//!
//! ```text
//! Acme-Windows-x64.zup
//! Acme-Windows-x64.zup.json     the small document that names it
//! ```
//!
//! After acquisition, every object inside is an ordinary verified CAS object at
//! a path derived from its digest. Nothing downstream can tell whether it came
//! from a package, a CDN, or a directory on a share — which is what makes
//! moving a project from GitHub-only distribution to a real CDN a configuration
//! change rather than a content migration.
//!
//! # Sharding
//!
//! Only when a package would exceed the host's per-asset limit, only at a fixed
//! boundary, and never visibly: a sharded package is
//! `Acme-Windows-x64.zup.000`, `.001`, and the acquisition engine reads them as
//! one source. A user-facing installer is never sharded, because a `Setup.exe`
//! that arrives as pieces is not the installer that was signed.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::Digest as _;
use zup_core::Sha256Digest;
use zup_distribute_github::{
    Document, FileRef, PackageDescriptor, PackageHeader, ShardRef, Writer,
};

/// Everything one package needs to be written.
pub struct Request<'a> {
    /// The variant's id, as the release names it.
    pub variant: &'a str,
    /// Its canonical target triple.
    pub target: &'a str,
    /// The application the release is for.
    pub application: &'a str,
    /// The variant manifest's own identity.
    pub manifest: Document,
    /// The content catalog's own identity.
    pub catalog: Document,
    /// The digests this variant needs, with their wire and logical sizes.
    pub blobs: &'a [(Sha256Digest, u64, u64)],
}

/// What writing one package produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The package's asset name, and the pieces if it was split.
    pub names: Vec<String>,
    /// The descriptor's asset name.
    pub descriptor: String,
    /// The package's total length.
    pub size: u64,
    /// How many blobs it holds.
    pub blob_count: u64,
    /// The sum of the blobs' logical sizes.
    pub logical_size: u64,
}

/// Write one variant's package, and the descriptor that names it.
///
/// Pieces are written to temporary names and renamed once the writer reports how
/// many there are, because whether the package is one file or several is a fact
/// the writer learns while it writes and a name cannot be chosen before it is
/// known. Two names for the same thing would be worse than a temporary file.
pub fn write(
    request: &Request<'_>,
    tree: &Path,
    output: &Path,
    application_name: &str,
    shard_bytes: u64,
) -> miette::Result<Written> {
    let name = zup_publish::package_name(application_name, request.variant);
    let mut writer = Writer::new();
    for (digest, compressed_size, size) in request.blobs {
        let wire = read_blob(tree, digest)
            .ok_or_else(|| miette::miette!("sha256:{digest} is not in the staged tree"))?;
        if wire.len() as u64 != *compressed_size {
            return Err(miette::miette!(
                "sha256:{digest} is {} bytes in the staged tree; the catalog says {compressed_size}",
                wire.len()
            ));
        }
        writer
            .insert(*digest, wire, *size)
            .map_err(|error| miette::miette!("{error}"))?;
    }
    std::fs::create_dir_all(output)
        .map_err(|error| miette::miette!("`{}`: {error}", output.display()))?;

    let mut digests: BTreeMap<usize, Sha256Digest> = BTreeMap::new();
    let mut sizes: BTreeMap<usize, u64> = BTreeMap::new();
    let header = PackageHeader::new(
        request.variant,
        request.target,
        request.application,
        request.manifest,
        request.catalog,
    );
    // The sink streams each piece straight to a file, so a package larger than
    // memory is written without ever being held whole.
    let metadata = writer
        .write(header, shard_bytes, |index, bytes| {
            let staged = staged_path(output, index);
            write_atomic(&staged, bytes)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            digests.insert(index, zup_core::hash_bytes(bytes));
            sizes.insert(index, bytes.len() as u64);
            Ok(())
        })
        .map_err(|error| miette::miette!("{error}"))?;

    // A package that fits its limit is one file with one name. A reader that had
    // to understand "a one-piece package is called something different" would
    // have two code paths for the ordinary case, and the two would eventually
    // disagree.
    let sharded = metadata.shards.len() > 1;
    let mut shard_refs = Vec::with_capacity(metadata.shards.len());
    for shard in &metadata.shards {
        let index = shard.index as usize;
        let written = sizes
            .get(&index)
            .copied()
            .ok_or_else(|| miette::miette!("the writer produced no piece {index}"))?;
        if written != shard.size {
            return Err(miette::miette!(
                "the writer reported piece {index} as {} bytes and wrote {written}",
                shard.size
            ));
        }
        let piece_name = if sharded {
            zup_publish::shard_name(&name, index)
        } else {
            name.clone()
        };
        let final_path = output.join(&piece_name);
        if final_path != staged_path(output, index) {
            std::fs::rename(staged_path(output, index), &final_path)
                .map_err(|error| miette::miette!("`{}`: {error}", final_path.display()))?;
        }
        shard_refs.push(ShardRef {
            index: shard.index,
            name: piece_name,
            digest: digests
                .get(&index)
                .copied()
                .ok_or_else(|| miette::miette!("the writer produced no piece {index}"))?,
            size: written,
            start: shard.start,
        });
    }
    let total: u64 = shard_refs.iter().map(|shard| shard.size).sum();
    let descriptor = PackageDescriptor {
        schema: zup_distribute_github::DESCRIPTOR_SCHEMA,
        variant: request.variant.to_owned(),
        target: request.target.to_owned(),
        application: request.application.to_owned(),
        package: FileRef {
            name: name.clone(),
            digest: package_digest(&shard_refs),
            size: total,
        },
        shards: shard_refs,
        blob_count: writer.len() as u64,
        logical_size: request
            .blobs
            .iter()
            .fold(0u64, |sum, (_, _, size)| sum + size),
    };
    descriptor
        .validate()
        .map_err(|error| miette::miette!("{error}"))?;
    let descriptor_name = zup_publish::document_name(&zup_publish::DocumentKind::Package {
        variant: request.variant,
    });
    let bytes = descriptor
        .encode()
        .map_err(|error| miette::miette!("{error}"))?;
    write_atomic(&output.join(&descriptor_name), &bytes)?;
    Ok(Written {
        names: descriptor
            .shards
            .iter()
            .map(|shard| shard.name.clone())
            .collect(),
        descriptor: descriptor_name,
        size: total,
        blob_count: descriptor.blob_count,
        logical_size: descriptor.logical_size,
    })
}

/// The whole package's identity, from its pieces.
///
/// For one piece that is the piece's digest, which is the whole package. For
/// several it is a digest *over the piece digests*: the concatenation cannot be
/// hashed without reading every byte, and a client that had to read every byte to
/// learn the package's identity would defeat the point of splitting it. Each
/// piece is verified on its own against the authenticated descriptor, which is
/// the property that actually matters.
fn package_digest(shards: &[ShardRef]) -> Sha256Digest {
    if shards.len() == 1 {
        return shards[0].digest;
    }
    let mut hasher = sha2::Sha256::new();
    for shard in shards {
        hasher.update(shard.digest.as_bytes());
    }
    Sha256Digest::from_hasher(hasher)
}

fn staged_path(output: &Path, index: usize) -> PathBuf {
    output.join(format!(".zup-piece-{index:04}"))
}

/// Read one blob's wire bytes from a staged tree.
pub fn read_blob(tree: &Path, digest: &Sha256Digest) -> Option<Vec<u8>> {
    let relative = zup_acquire::WebLayout::blob(digest).to_string();
    let mut path = tree.to_path_buf();
    for segment in relative.split('/') {
        path.push(segment);
    }
    std::fs::read(path).ok()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> miette::Result<()> {
    let temporary = temporary_path(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| miette::miette!("`{}`: {error}", parent.display()))?;
    }
    {
        let mut file = std::fs::File::create(&temporary)
            .map_err(|error| miette::miette!("`{}`: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| miette::miette!("`{}`: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| miette::miette!("`{}`: {error}", temporary.display()))?;
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        miette::miette!("`{}`: {error}", path.display())
    })
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_owned());
    name.push_str(".zup-tmp");
    path.with_file_name(name)
}
