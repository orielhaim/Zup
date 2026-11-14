use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use zup_artifact::ReleaseManifest;
use zup_distribute_github::{Index, PackageDescriptor};
use zup_publish::{ProductClass, ProductRole, ReleaseProduct};

pub(super) fn file(root: &Path, relative: &str) -> miette::Result<PathBuf> {
    if relative.contains(['\\', ':'])
        || relative
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(miette::miette!("invalid release path `{relative}`"));
    }
    let mut path = root.to_path_buf();
    for ancestor in std::path::absolute(root)
        .map_err(|e| miette::miette!("release root: {e}"))?
        .ancestors()
    {
        if is_link(
            &std::fs::symlink_metadata(ancestor)
                .map_err(|e| miette::miette!("release ancestor: {e}"))?,
        ) {
            return Err(miette::miette!("release root has a linked ancestor"));
        }
    }
    let root_metadata =
        std::fs::symlink_metadata(root).map_err(|e| miette::miette!("release directory: {e}"))?;
    if is_link(&root_metadata) {
        return Err(miette::miette!("release directory is a link"));
    }
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(miette::miette!("invalid release path `{relative}`"));
        };
        path.push(name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
        if is_link(&metadata) {
            return Err(miette::miette!(
                "release path `{}` is a link",
                path.display()
            ));
        }
    }
    if !path.is_file() {
        return Err(miette::miette!(
            "`{}` is not a regular file",
            path.display()
        ));
    }
    Ok(path)
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn packages(
    release: &ReleaseManifest,
    root: &Path,
    web: Option<&Path>,
) -> miette::Result<Vec<(ReleaseProduct, PathBuf)>> {
    let mut result = BTreeMap::new();
    let mut variants = BTreeSet::new();
    for transport in &release.transport {
        let variant = release
            .variants
            .iter()
            .find(|v| v.id == transport.variant)
            .ok_or_else(|| {
                miette::miette!("transport names foreign variant `{}`", transport.variant)
            })?;
        if !variants.insert(&transport.variant) {
            return Err(miette::miette!(
                "duplicate transport variant `{}`",
                transport.variant
            ));
        }
        let path = file(root, &transport.descriptor)?;
        if transport.identity.size > zup_distribute_github::MAX_DESCRIPTOR_BYTES as u64 {
            return Err(miette::miette!("package descriptor exceeds one MiB"));
        }
        let bytes = std::fs::read(&path).map_err(|e| miette::miette!("descriptor: {e}"))?;
        if bytes.len() as u64 != transport.identity.size {
            return Err(miette::miette!("descriptor size mismatch"));
        }
        let descriptor = PackageDescriptor::parse(&bytes, Some(transport.identity.digest))
            .map_err(|e| miette::miette!("{e}"))?;
        if descriptor.variant != variant.id
            || descriptor.target != variant.target.as_str()
            || descriptor.application != release.application.id.as_str()
            || descriptor.document_name() != transport.descriptor
        {
            return Err(miette::miette!("transport descriptor identity mismatch"));
        }
        insert(
            &mut result,
            ReleaseProduct::new(
                &transport.descriptor,
                ProductRole::Manifest,
                ProductClass::Metadata,
                transport.identity.digest,
                transport.identity.size,
            )
            .serving([variant.id.clone()])
            .requiring(descriptor.shards.iter().map(|s| s.name.clone()))
            .with_media_type("application/json"),
            path,
        )?;
        let mut prefix = Vec::new();
        use sha2::Digest as _;
        let mut logical = sha2::Sha256::new();
        for shard in &descriptor.shards {
            let path = file(root, &shard.name)?;
            let (size, digest) = super::measure(&path)?;
            if size != shard.size || digest != shard.digest {
                return Err(miette::miette!(
                    "transport shard `{}` size or digest mismatch",
                    shard.name
                ));
            }
            use std::io::Read as _;
            let mut input =
                std::fs::File::open(&path).map_err(|e| miette::miette!("package: {e}"))?;
            let mut buffer = [0u8; 65536];
            loop {
                let read = input
                    .read(&mut buffer)
                    .map_err(|e| miette::miette!("package: {e}"))?;
                if read == 0 {
                    break;
                }
                logical.update(&buffer[..read]);
            }
            if shard.index == 0 {
                use std::io::Read as _;
                std::fs::File::open(&path)
                    .and_then(|f| {
                        f.take(
                            zup_distribute_github::MAX_METADATA
                                + zup_distribute_github::HEADER_LEN as u64,
                        )
                        .read_to_end(&mut prefix)
                    })
                    .map_err(|e| miette::miette!("package index: {e}"))?;
            }
            insert(
                &mut result,
                ReleaseProduct::new(
                    &shard.name,
                    ProductRole::Auxiliary,
                    ProductClass::Transport,
                    digest,
                    size,
                )
                .serving([variant.id.clone()]),
                path,
            )?;
        }
        if zup_core::Sha256Digest::from_hasher(logical) != descriptor.package.digest {
            return Err(miette::miette!("logical package digest mismatch"));
        }
        let index = Index::read(&prefix).map_err(|e| miette::miette!("{e}"))?;
        if index.metadata.variant != descriptor.variant
            || index.metadata.target != descriptor.target
            || index.metadata.application != descriptor.application
            || index.total != descriptor.package.size
            || index
                .metadata
                .shards
                .iter()
                .map(|s| (s.index, s.start, s.size))
                .collect::<Vec<_>>()
                != descriptor
                    .shards
                    .iter()
                    .map(|s| (s.index, s.start, s.size))
                    .collect::<Vec<_>>()
        {
            return Err(miette::miette!("package index differs from descriptor"));
        }
        if let Some(web) = web {
            verify_catalog(release, web, &index)?;
        }
        verify_frames(&index, &descriptor, root)?;
    }
    let actual = std::fs::read_dir(root)
        .map_err(|e| miette::miette!("packages: {e}"))?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|e| miette::miette!("packages: {e}"))?;
    if actual != result.keys().cloned().collect() {
        return Err(miette::miette!(
            "package directory differs from the release inventory; restage into an empty directory"
        ));
    }
    Ok(result.into_values().collect())
}

pub(super) fn web_closure(release: &ReleaseManifest, web: &Path) -> miette::Result<()> {
    let roots = release
        .documents
        .iter()
        .filter(|d| d.path.starts_with("releases/"))
        .collect::<Vec<_>>();
    if roots.is_empty() && release.documents.is_empty() && release.objects.is_empty() {
        return Ok(());
    }
    if roots.len() != 1 {
        return Err(miette::miette!(
            "release inventory requires exactly one web release graph"
        ));
    }
    let bytes = std::fs::read(file(web, &roots[0].path)?)
        .map_err(|e| miette::miette!("release graph: {e}"))?;
    if (bytes.len() as u64, zup_core::hash_bytes(&bytes))
        != (roots[0].identity.size, roots[0].identity.digest)
    {
        return Err(miette::miette!("release graph identity mismatch"));
    }
    let graph =
        zup_acquire::ReleaseDescriptor::parse(&bytes).map_err(|e| miette::miette!("{e}"))?;
    if graph.app_id != release.application.id
        || graph.version != release.application.version.to_string()
        || graph
            .variants
            .iter()
            .map(|v| (&v.id, v.target))
            .collect::<BTreeSet<_>>()
            != release.variants.iter().map(|v| (&v.id, v.target)).collect()
    {
        return Err(miette::miette!(
            "web graph application or target identity mismatch"
        ));
    }
    let mut expected = BTreeMap::new();
    let mut required = BTreeSet::new();
    let mut documents = BTreeSet::from([
        roots[0].path.clone(),
        zup_acquire::document_path(&zup_core::hash_bytes(&bytes)).to_string(),
        zup_acquire::document_path(&graph.catalog.digest).to_string(),
    ]);
    for variant in &graph.variants {
        documents.insert(zup_acquire::document_path(&variant.manifest.digest).to_string());
        required.extend(variant.content.iter().copied());
        for (reference, kind) in [
            (variant.runtime.as_ref(), zup_artifact::MediaType::Runtime),
            (variant.preset.as_ref(), zup_artifact::MediaType::Preset),
        ] {
            if let Some(reference) = reference {
                required.insert(reference.digest);
                let entry = expected.entry(reference.digest).or_insert((
                    reference.size,
                    kind,
                    BTreeSet::new(),
                ));
                if entry.0 != reference.size || entry.1 != kind {
                    return Err(miette::miette!("conflicting native object roles"));
                }
                entry.2.insert(variant.id.clone());
            }
        }
    }
    let actual = release
        .objects
        .iter()
        .map(|o| {
            (
                o.identity.digest,
                (
                    o.identity.size,
                    o.media_type,
                    o.variants.iter().cloned().collect::<BTreeSet<_>>(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if actual != expected {
        return Err(miette::miette!(
            "native object inventory does not close the release graph"
        ));
    }
    if release
        .documents
        .iter()
        .filter(|d| !d.path.starts_with("metadata/"))
        .map(|d| d.path.clone())
        .collect::<BTreeSet<_>>()
        != documents
    {
        return Err(miette::miette!(
            "document inventory does not close the release graph"
        ));
    }
    let catalog_bytes = std::fs::read(file(
        web,
        &zup_acquire::document_path(&graph.catalog.digest).to_string(),
    )?)
    .map_err(|e| miette::miette!("catalog: {e}"))?;
    if (
        catalog_bytes.len() as u64,
        zup_core::hash_bytes(&catalog_bytes),
    ) != (graph.catalog.size, graph.catalog.digest)
    {
        return Err(miette::miette!("catalog identity mismatch"));
    }
    let catalog =
        zup_acquire::ContentCatalog::parse(&catalog_bytes).map_err(|e| miette::miette!("{e}"))?;
    if catalog.digests().copied().collect::<BTreeSet<_>>() != required {
        return Err(miette::miette!(
            "catalog contains missing or foreign content"
        ));
    }
    let expected_blobs = required
        .iter()
        .map(|d| web.join(zup_acquire::blob_path(d).to_string()))
        .collect::<BTreeSet<_>>();
    let mut actual_blobs = BTreeSet::new();
    collect_files(&web.join("blobs"), &mut actual_blobs)?;
    if actual_blobs != expected_blobs {
        return Err(miette::miette!(
            "staged blob files differ from the finite catalog"
        ));
    }
    Ok(())
}

fn collect_files(directory: &Path, out: &mut BTreeSet<PathBuf>) -> miette::Result<()> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(miette::miette!("staged content: {e}")),
    };
    if is_link(&metadata) || !metadata.is_dir() {
        return Err(miette::miette!(
            "staged namespace is not a regular directory"
        ));
    }
    for entry in std::fs::read_dir(directory).map_err(|e| miette::miette!("staged content: {e}"))? {
        let path = entry
            .map_err(|e| miette::miette!("staged content: {e}"))?
            .path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| miette::miette!("staged content: {e}"))?;
        if is_link(&metadata) {
            return Err(miette::miette!("staged content contains a link"));
        }
        if metadata.is_dir() {
            collect_files(&path, out)?;
        } else if metadata.is_file() {
            out.insert(path);
        } else {
            return Err(miette::miette!("staged content is not regular"));
        }
    }
    Ok(())
}

fn verify_frames(index: &Index, descriptor: &PackageDescriptor, root: &Path) -> miette::Result<()> {
    use std::io::{Read as _, Seek as _};
    let mut cursor = index.frames_start;
    let mut logical = 0u64;
    for frame in &index.metadata.blobs {
        let shard = descriptor
            .shards
            .iter()
            .find(|s| frame.offset >= s.start && frame.offset < s.start.saturating_add(s.size))
            .ok_or_else(|| miette::miette!("frame is outside its physical shards"))?;
        let end = frame
            .offset
            .checked_add(frame.compressed_size)
            .ok_or_else(|| miette::miette!("frame size overflow"))?;
        if frame.offset != cursor
            || frame.compressed_size == 0
            || end > shard.start.saturating_add(shard.size)
        {
            return Err(miette::miette!(
                "package frame ranges leave missing, extra, or overlapping bytes"
            ));
        }
        let mut input = std::fs::File::open(file(root, &shard.name)?)
            .map_err(|e| miette::miette!("frame: {e}"))?;
        input
            .seek(std::io::SeekFrom::Start(frame.offset - shard.start))
            .map_err(|e| miette::miette!("frame: {e}"))?;
        let decoded = zstd::stream::read::Decoder::new(input.take(frame.compressed_size))
            .map_err(|e| miette::miette!("frame: {e}"))?;
        let identity = zup_core::hash_reader(decoded.take(frame.size.saturating_add(1)))
            .map_err(|e| miette::miette!("frame: {e}"))?;
        if identity != (frame.size, frame.digest) {
            return Err(miette::miette!(
                "package frame logical digest or size mismatch"
            ));
        }
        cursor = end;
        logical = logical
            .checked_add(frame.size)
            .ok_or_else(|| miette::miette!("logical size overflow"))?;
    }
    if cursor != descriptor.package.size
        || logical != descriptor.logical_size
        || index.metadata.blobs.len() as u64 != descriptor.blob_count
    {
        return Err(miette::miette!(
            "package content does not close its descriptor"
        ));
    }
    Ok(())
}

fn verify_catalog(release: &ReleaseManifest, web: &Path, index: &Index) -> miette::Result<()> {
    let roots = release
        .documents
        .iter()
        .filter(|d| d.path.starts_with("releases/"))
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err(miette::miette!(
            "transport requires exactly one staged release graph"
        ));
    }
    let bytes = std::fs::read(file(web, &roots[0].path)?)
        .map_err(|e| miette::miette!("release graph: {e}"))?;
    if (bytes.len() as u64, zup_core::hash_bytes(&bytes))
        != (roots[0].identity.size, roots[0].identity.digest)
    {
        return Err(miette::miette!("release graph identity mismatch"));
    }
    let graph =
        zup_acquire::ReleaseDescriptor::parse(&bytes).map_err(|e| miette::miette!("{e}"))?;
    let variant = graph
        .variants
        .iter()
        .find(|v| v.id == index.metadata.variant)
        .ok_or_else(|| miette::miette!("package variant is absent from the release graph"))?;
    if graph.app_id != release.application.id
        || graph.version != release.application.version.to_string()
        || variant.target.as_str() != index.metadata.target
        || (variant.manifest.digest, variant.manifest.size)
            != (index.metadata.manifest.digest, index.metadata.manifest.size)
        || (graph.catalog.digest, graph.catalog.size)
            != (index.metadata.catalog.digest, index.metadata.catalog.size)
        || variant.content.iter().copied().collect::<BTreeSet<_>>()
            != index.metadata.blobs.iter().map(|f| f.digest).collect()
    {
        return Err(miette::miette!(
            "transport differs from the staged release graph"
        ));
    }
    for reference in [&variant.manifest, &graph.catalog] {
        if !release
            .documents
            .iter()
            .any(|d| d.identity.digest == reference.digest && d.identity.size == reference.size)
        {
            return Err(miette::miette!(
                "release inventory is missing a required document"
            ));
        }
    }
    for reference in [variant.runtime.as_ref(), variant.preset.as_ref()]
        .into_iter()
        .flatten()
    {
        if !release.objects.iter().any(|o| {
            o.identity.digest == reference.digest
                && o.identity.size == reference.size
                && o.variants.contains(&variant.id)
        }) {
            return Err(miette::miette!(
                "release inventory is missing a required native object"
            ));
        }
    }
    let relative = zup_acquire::document_path(&graph.catalog.digest).to_string();
    let bytes =
        std::fs::read(file(web, &relative)?).map_err(|e| miette::miette!("catalog: {e}"))?;
    if (bytes.len() as u64, zup_core::hash_bytes(&bytes))
        != (graph.catalog.size, graph.catalog.digest)
    {
        return Err(miette::miette!("catalog identity mismatch"));
    }
    let catalog = zup_acquire::ContentCatalog::parse(&bytes).map_err(|e| miette::miette!("{e}"))?;
    for frame in &index.metadata.blobs {
        if !catalog.entry(&frame.digest).is_some_and(|entry| {
            entry.size == frame.size
                && entry.compressed_size == frame.compressed_size
                && entry.compression == zup_acquire::ContentCompression::Zstandard
        }) {
            return Err(miette::miette!(
                "package frame differs from its content catalog"
            ));
        }
    }
    Ok(())
}

fn insert(
    result: &mut BTreeMap<String, (ReleaseProduct, PathBuf)>,
    product: ReleaseProduct,
    path: PathBuf,
) -> miette::Result<()> {
    zup_publish::check_asset_name(&product.name).map_err(|e| miette::miette!("{e}"))?;
    if result
        .insert(product.name.clone(), (product, path))
        .is_some()
    {
        return Err(miette::miette!("duplicate physical transport asset"));
    }
    Ok(())
}
