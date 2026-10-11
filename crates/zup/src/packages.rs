//! boundary, and never visibly: a sharded package is
//! one source. A user-facing installer is never sharded, because a `Setup.exe`

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::Digest as _;
use zup_core::Sha256Digest;
use zup_distribute_github::{
    Document, FileRef, PackageDescriptor, PackageHeader, ShardRef, Writer,
};

pub struct Request<'a> {
    pub variant: &'a str,
    pub target: &'a str,
    pub application: &'a str,
    pub manifest: Document,
    pub catalog: Document,
    pub blobs: &'a [(Sha256Digest, u64, u64)],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub names: Vec<String>,
    pub descriptor: String,
    pub size: u64,
    pub blob_count: u64,
    pub logical_size: u64,
}

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
