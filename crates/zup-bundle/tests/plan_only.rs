//! The plan-only package: a native runtime that carries its plan and no payload.
//!
//! This is the shape a thin installer's runtime has to be. If the runtime
//! embedded the payload it would be the whole application, and a thin installer
//! would be a slow offline installer wearing a different name.

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use zup_acquire::{CachePolicy, CatalogEntry, ContentCache, ContentCatalog};
use zup_build::TargetBuildPlan;
use zup_bundle::{BundleWriter, Package, PayloadSource};
use zup_core::{RelativePath, Sha256Digest, hash_reader};
use zup_manifest::TargetOverrides;

fn plan(root: &Path) -> TargetBuildPlan {
    fs::create_dir_all(root.join("dist")).unwrap();
    // Large enough that the difference between a plan and a package is a real
    // number rather than a rounding artefact, partly shared so the content set is
    // genuinely deduplicated, and incompressible so the wire form is the logical
    // form and the comparison is not measuring zstd.
    let big = incompressible(512 * 1024, 0x9e37_79b9);
    let other = incompressible(256 * 1024, 0x85eb_ca6b);
    fs::write(root.join("dist/a.bin"), &big).unwrap();
    fs::write(root.join("dist/b.bin"), &big).unwrap();
    fs::write(root.join("dist/c.bin"), &other).unwrap();
    let manifest = format!(
        r#"
schema = 1
[app]
id = "com.example.thin"
name = "Thin Test"
version = "1.0.0"
[build]
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/ThinTest"
[[files]]
source = "**/*"
destination = "${{install}}"
"#,
        target = zup_plugin_contract::HOST_TARGET
    );
    let parsed = zup_manifest::parse(&manifest).unwrap();
    let installer = zup_manifest::parse_and_compile(&manifest, "default").unwrap();
    let config = zup_manifest::select_targets(&parsed, &["default"], &TargetOverrides::default())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut build = zup_build::materialize(
        &root.join("zup.toml"),
        &parsed,
        vec![(config, installer)],
        zup_build::Writes::None,
    )
    .unwrap();
    build.targets.pop().unwrap()
}

/// Bytes a compressor cannot shrink, from a fixed generator so a failure is
/// reproducible.
fn incompressible(size: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..size)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u8
        })
        .collect()
}

/// A cache holding every blob a plan needs, as the web tree would serve them.
fn seeded_cache(root: &Path, plan: &TargetBuildPlan) -> (ContentCache, ContentCatalog) {
    let cache = ContentCache::open(root.join("cache"), CachePolicy::Auto).unwrap();
    let mut entries = Vec::new();
    for file in &plan.files {
        let logical = fs::read(&file.source).unwrap();
        let wire = zstd::stream::encode_all(logical.as_slice(), 9).unwrap();
        let descriptor = zup_acquire::ContentDescriptor::compressed(
            zup_acquire::ContentKind::Payload,
            file.sha256,
            wire.len() as u64,
            file.size,
        );
        let mut writer = cache.writer(&descriptor).unwrap();
        writer.write(&wire).unwrap();
        writer.commit().unwrap();
        entries.push(CatalogEntry::compressed(
            file.sha256,
            wire.len() as u64,
            file.size,
        ));
    }
    (cache, ContentCatalog::new(entries).unwrap())
}

#[test]
fn a_plan_only_package_carries_the_plan_and_none_of_the_content() {
    let root = TempDir::new().unwrap();
    let plan = plan(root.path());
    let full = BundleWriter::encode(&plan, &[]).unwrap();
    let thin = BundleWriter::encode_plan_only(&plan, &[]).unwrap();

    assert!(
        thin.len() * 100 < full.len(),
        "a thin runtime must not be a repackaged application: {} vs {}",
        thin.len(),
        full.len()
    );

    let package = Package::parse(&thin).expect("a plan-only package parses");
    assert!(package.is_plan_only());
    assert_eq!(package.plan(), Package::parse(&full).unwrap().plan());
    // The plan is complete: this is what lets the runtime plan a lifecycle with
    // no manifest and no source directory.
    assert_eq!(package.plan().entries.len(), 3);
    assert!(package.build_plan().is_ok());
    assert_eq!(
        package.required_digests().len(),
        2,
        "a deduplicated content set: two of the three files share content"
    );

    // It carries no blob index, so a reader cannot believe it holds the bytes.
    assert!(package.blob_count() == 0);
    assert!(zup_bundle::refuse_self_verified(package.is_plan_only()).is_err());
    assert!(zup_bundle::refuse_self_verified(false).is_ok());
}

#[test]
fn a_plan_only_package_serves_its_content_from_a_verified_cache() {
    let root = TempDir::new().unwrap();
    let plan = plan(root.path());
    let thin = BundleWriter::encode_plan_only(&plan, &[]).unwrap();
    let package = Package::parse(&thin).unwrap();
    let (cache, catalog) = seeded_cache(root.path(), &plan);
    let source = zup_bundle::AcquiredPayloadSource::new(cache, catalog, &package);

    for entry in &package.plan().entries {
        let mut reader = source
            .open(&entry.path, &entry.sha256, entry.size)
            .unwrap_or_else(|error| panic!("{}: {error}", entry.path));
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut out).unwrap();
        let (_, digest) = hash_reader(out.as_slice()).unwrap();
        assert_eq!(digest, entry.sha256, "{}", entry.path);
        assert_eq!(out.len() as u64, entry.size);
    }
}

#[test]
fn a_tampered_cache_object_never_becomes_payload() {
    // The cache holds a wire form whose name is a digest. Corrupting it must
    // fail at the source, not produce a file.
    let root = TempDir::new().unwrap();
    let plan = plan(root.path());
    let thin = BundleWriter::encode_plan_only(&plan, &[]).unwrap();
    let package = Package::parse(&thin).unwrap();
    let (cache, catalog) = seeded_cache(root.path(), &plan);
    let source = zup_bundle::AcquiredPayloadSource::new(cache, catalog.clone(), &package);

    let victim = &package.plan().entries[0];
    let relative = zup_acquire::blob_path(&victim.sha256).to_string();
    let mut path = source_cache_root(root.path());
    for segment in relative.split('/') {
        path.push(segment);
    }
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();

    // The wire length still matches, so this is exactly the case the payload
    // verification policy is for: identity is not proved by the file's name.
    let entry = catalog.entry(&victim.sha256).expect("a catalog entry");
    assert_eq!(entry.compressed_size, bytes.len() as u64);

    // A corrupted frame is caught either when the decompressor reads its header
    // or when the logical digest is proved at the end. Both are refusals, and
    // neither produces a file.
    match source.open(&victim.path, &victim.sha256, victim.size) {
        Err(_) => {}
        Ok(mut reader) => {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut out)
                .expect_err("a corrupted wire form cannot be read as payload");
        }
    }
}

#[test]
fn a_plan_only_package_refuses_a_path_its_plan_does_not_name() {
    let root = TempDir::new().unwrap();
    let plan = plan(root.path());
    let package = Package::parse(
        BundleWriter::encode_plan_only(&plan, &[])
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    let (cache, catalog) = seeded_cache(root.path(), &plan);
    let source = zup_bundle::AcquiredPayloadSource::new(cache, catalog, &package);
    let stranger = RelativePath::new("not-in-the-plan.bin").unwrap();
    assert!(
        source
            .open(&stranger, &Sha256Digest::from_bytes([0; 32]), 4)
            .is_err()
    );
}

fn source_cache_root(root: &Path) -> std::path::PathBuf {
    // The cache is created at `<root>/cache`, and a blob lives under
    // `<cache>/blobs/sha256/...`; this recomputes that without exposing the
    // cache's root to the test.
    let probe = ContentCache::open(root.join("cache"), CachePolicy::Auto).unwrap();
    probe.root().to_path_buf()
}
