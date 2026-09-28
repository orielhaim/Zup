//! The handoff, bounded and refused.
//!
//! A thin bootstrapper resolves a release, verifies a native runtime, and starts
//! it. Two things about that moment are worth pinning.
//!
//! - **The bound.** A handoff is a few hundred bytes, and everything the runtime
//!   does before it draws anything is reading that document and hashing one file.
//!   The window appears afterwards, so those are the whole of what a user waits
//!   for.
//! - **The claim.** The bootstrapper is the untrusted half of the pair, because
//!   it is the half that talked to a network. Every substitution it could attempt
//!   is attempted here and refused.
//!
//! The runtime identity is a real file on disk and the release is a real signed
//! document tree, so nothing here is a stand-in for the thing it asserts.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_acquire::{
    CachePolicy, ContentCatalog, ContentDescriptor, ContentKind, DocumentRef, HandoffMode,
    RELEASE_SCHEMA, ReleaseDescriptor, ReleaseVariant, RuntimeHandoff, SessionSummary,
};
use zup_core::Sha256Digest;

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

/// A payload blob of incompressible content.
fn payload(seed: u64, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for slot in out.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
    out
}

/// A real image on disk to stand in for the runtime.
///
/// A real PE, because two of the checks are about the image rather than about a
/// digest: `verify` reads this executable's own target triple, and a file of
/// random bytes would fail that for the wrong reason. The test binary is a real
/// x86_64 console PE, which is exactly what a native runtime is.
///
/// `salt` produces a *different* image that is still a valid PE - appending to
/// the end of an image leaves its headers intact - so a substitution is a genuine
/// second file rather than the same bytes under a second name.
fn runtime_image(root: &Path, name: &str, salt: u64) -> PathBuf {
    let path = root.join(name);
    let mut bytes = std::fs::read(std::env::current_exe().expect("the test executable"))
        .expect("the test executable is readable");
    if salt != 0 {
        bytes.extend_from_slice(&payload(salt, 64));
    }
    std::fs::write(&path, &bytes).expect("the image is written");
    path
}

/// A release, its documents, and a cache that holds them.
struct Fixture {
    dir: TempDir,
    cache: zup_acquire::ContentCache,
    runtime: PathBuf,
    release: ReleaseDescriptor,
    /// The digest of the release document's bytes, which is the cache key and
    /// the one field a bootstrapper may choose freely.
    document: Sha256Digest,
    manifest_bytes: Vec<u8>,
    content: Vec<ContentDescriptor>,
}

/// The plan a real `zup publish stage --thin` compiles into a runtime, which is
/// what the handoff's `manifest` field names, and the digests of the content it
/// installs.
///
/// The project lives in the fixture's own directory, so two tests never contend
/// for one scratch path.
fn plan_for(root: &Path, files: usize) -> (Vec<u8>, Vec<Sha256Digest>) {
    let project = root.join("project");
    std::fs::create_dir_all(project.join("dist")).expect("a source directory");
    let mut digests = Vec::new();
    for index in 0..files {
        let bytes = payload(index as u64 + 100, 16 * 1024);
        digests.push(digest_of(&bytes));
        std::fs::write(project.join(format!("dist/file{index}.bin")), &bytes).expect("a file");
    }
    std::fs::write(
        project.join("zup.toml"),
        r#"
schema = 1
frontend = "gui"
[app]
id = "com.example.handoff"
name = "Handoff"
version = "1.4.0"
[updates]
repository = "https://updates.example.com/handoff"
channel = "stable"
root = "root.json"
[build]

[build.targets.x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"
[install.directory]
user = "${location.programs}/Handoff"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .expect("a manifest");
    std::fs::write(
        project.join("root.json"),
        include_str!("fixtures/root.json"),
    )
    .expect("a root");

    let source = std::fs::read_to_string(project.join("zup.toml")).expect("a manifest");
    let manifest = zup_manifest::parse(&source).expect("the manifest parses");
    let config = zup_manifest::select_targets(
        &manifest,
        &["x64"],
        &zup_manifest::TargetOverrides::default(),
    )
    .expect("a selection")
    .into_iter()
    .next()
    .expect("one target");
    let installer = zup_manifest::compile(
        &manifest,
        &config,
        &zup_manifest::TargetOverrides::default(),
    )
    .expect("a compiled installer");
    let build = zup_build::materialize(
        &project.join("zup.toml"),
        &manifest,
        vec![(config.clone(), installer)],
    )
    .expect("a build plan");
    let target = build.targets.first().expect("one target");
    let variant =
        zup_artifact::DistributionVariant::resolve(&config, target, &[], None).expect("a variant");
    let bytes = zup_artifact::VariantManifest::encode(&variant).expect("an encoded manifest");
    let mut sorted = digests;
    sorted.sort_unstable();
    (bytes, sorted)
}

fn fixture(files: usize) -> Fixture {
    let dir = TempDir::new().expect("a temporary directory");
    let cache_root = dir.path().join("content");
    let cache = zup_acquire::ContentCache::open(&cache_root, CachePolicy::Auto).expect("a cache");
    let runtime = runtime_image(dir.path(), "Setup.exe", 42);
    let runtime_digest = zup_windows::own_digest(&runtime).expect("the runtime hashes");

    let (manifest_bytes, planned) = plan_for(dir.path(), files);
    let manifest_digest = digest_of(&manifest_bytes);

    // The content the release serves, in the cache under its own digests.
    let mut content = Vec::new();
    let mut entries = Vec::new();
    for index in 0..files {
        let logical = payload(index as u64 + 100, 16 * 1024);
        let wire = zstd::stream::encode_all(logical.as_slice(), 9).expect("the fixture compresses");
        let descriptor = ContentDescriptor::compressed(
            ContentKind::Payload,
            digest_of(&logical),
            wire.len() as u64,
            logical.len() as u64,
        );
        let mut writer = cache.writer(&descriptor).expect("a writer");
        writer.write(&wire).expect("a write");
        writer.commit().expect("published");
        content.push(descriptor);
        entries.push(zup_acquire::CatalogEntry::compressed(
            descriptor.digest,
            descriptor.compressed_size,
            descriptor.size,
        ));
    }
    let catalog = ContentCatalog::new(entries).expect("a catalog");
    let catalog_bytes = catalog.encode().expect("the catalog encodes");
    let catalog_digest = digest_of(&catalog_bytes);

    let target = zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a triple");
    let mut release = ReleaseDescriptor {
        schema: RELEASE_SCHEMA,
        app_id: zup_core::AppId::new("com.example.handoff").expect("valid"),
        channel: "stable".to_owned(),
        version: "1.4.0".to_owned(),
        release_digest: Sha256Digest::from_bytes([0; 32]),
        catalog: DocumentRef::of(catalog_digest, catalog_bytes.len() as u64),
        variants: vec![ReleaseVariant {
            id: "x64".to_owned(),
            target,
            platform: "windows".to_owned(),
            frontend: "gui".to_owned(),
            manifest: DocumentRef::of(manifest_digest, manifest_bytes.len() as u64),
            runtime: Some(DocumentRef::of(
                runtime_digest,
                std::fs::metadata(&runtime).expect("a runtime").len(),
            )),
            // The content set is the one the plan names, which is what a real
            // release carries: a graph that disagreed with its own manifest would
            // be a defect the client would have to guess about.
            content: planned,
            requirements: Default::default(),
            logical_size: content.iter().map(|descriptor| descriptor.size).sum(),
        }],
        downloads: Vec::new(),
    };
    release.release_digest = release.computed_digest().expect("fingerprints");

    // Documents are cached under their own digests, which is how the runtime
    // re-reads and re-hashes them. The release is addressed by the digest of its
    // *bytes*, which is not its own fingerprint: a release document names that
    // fingerprint as a field, so it cannot also be the hash of its encoding.
    let release_bytes = release.encode().expect("the release encodes");
    let document = digest_of(&release_bytes);
    for (digest, bytes) in [
        (catalog_digest, catalog_bytes.as_slice()),
        (manifest_digest, manifest_bytes.as_slice()),
        (document, release_bytes.as_slice()),
    ] {
        let mut writer = cache
            .writer(&ContentDescriptor::stored(
                ContentKind::Metadata,
                digest,
                bytes.len() as u64,
            ))
            .expect("a writer");
        writer.write(bytes).expect("a write");
        writer.commit().expect("published");
    }

    Fixture {
        dir,
        cache,
        runtime,
        release,
        document,
        manifest_bytes,
        content,
    }
}

fn handoff_for(fixture: &Fixture) -> RuntimeHandoff {
    let variant = &fixture.release.variants[0];
    RuntimeHandoff {
        schema: zup_acquire::HANDOFF_SCHEMA,
        app_id: fixture.release.app_id.clone(),
        release: fixture.release.release_digest,
        document: fixture.document,
        catalog: fixture.release.catalog.digest,
        variant: variant.id.clone(),
        manifest: variant.manifest.digest,
        runtime: variant.runtime.expect("a runtime").digest,
        target: variant.target.clone(),
        frontend: variant.frontend.clone(),
        mode: HandoffMode::Install,
        scope: "user".to_owned(),
        session: SessionSummary {
            total_bytes: fixture
                .content
                .iter()
                .map(|descriptor| descriptor.compressed_size)
                .sum(),
            cached_bytes: 0,
            cache_hits: 0,
            downloaded_items: fixture.content.len() as u64,
            elapsed_ms: 3_100,
        },
        components: Vec::new(),
    }
}

fn write_handoff(fixture: &Fixture, handoff: &RuntimeHandoff) -> PathBuf {
    let path = fixture.dir.path().join("handoff.json");
    let bytes = handoff.encode().expect("the handoff encodes");
    std::fs::write(&path, &bytes).expect("the handoff is written");
    path
}

#[test]
fn accepting_a_handoff_proves_the_image_the_release_names() {
    let fixture = fixture(24);
    let handoff = handoff_for(&fixture);
    let path = write_handoff(&fixture, &handoff);
    let bytes = std::fs::metadata(&path).expect("a handoff").len();

    let accepted = zup_windows::accept_handoff(&path, Some(handoff.digest()))
        .unwrap_or_else(|error| panic!("the handoff is accepted: {error}"));
    let baseline = zup_core::hash_reader(
        std::fs::File::open(&fixture.runtime).expect("the runtime is readable"),
    )
    .expect("the runtime hashes");
    let verified = zup_windows::verify(&fixture.runtime, &fixture.cache, &accepted)
        .unwrap_or_else(|error| panic!("the runtime is the one the release names: {error}"));

    // The window appears only after this, so what a user waits for between
    // double-clicking and seeing the installer is the reading of this document and
    // one pass over the image. Both are bounded by the architecture, not by the
    // release, which is why a handoff carries digests rather than contents.
    assert!(
        bytes < 4096,
        "a handoff is a few hundred bytes, not a plan: {bytes}"
    );
    assert_eq!(verified.catalog.blobs.len(), fixture.content.len());
    assert_eq!(verified.manifest, fixture.manifest_bytes);
    assert_eq!(
        verified.release.release_digest,
        fixture.release.release_digest
    );
    assert_eq!(
        verified.handoff.runtime, baseline.1,
        "the image the release names is the one on disk"
    );
}

#[test]
fn a_bootstrapper_cannot_substitute_anything_the_release_authenticated() {
    // Each case is a substitution the untrusted half of the pair could attempt.
    // Every one has to be refused by the runtime, and every refusal happens before
    // the transaction engine is asked for a plan.
    let fixture = fixture(8);
    let honest = handoff_for(&fixture);
    let path = write_handoff(&fixture, &honest);
    zup_windows::verify(&fixture.runtime, &fixture.cache, &honest)
        .unwrap_or_else(|error| panic!("the honest handoff verifies: {error}"));

    // 1. A different runtime image. The release names this image's digest, so a
    //    substituted image does not hash to it.
    let other = runtime_image(fixture.dir.path(), "Other.exe", 43);
    let stranger = zup_windows::own_digest(&other).expect("the stranger hashes");
    let mut substituted = honest.clone();
    substituted.runtime = stranger;
    let error = zup_windows::verify(&other, &fixture.cache, &substituted)
        .expect_err("a substituted runtime is refused");
    assert!(error.left_machine_unchanged());
    println!("\nsubstitutions");
    println!("  runtime           refused: {error}");

    // 2. A different variant. The release has to name it, and it does not.
    let mut substituted = honest.clone();
    substituted.variant = "aarch64".to_owned();
    let error =
        zup_windows::verify(&fixture.runtime, &fixture.cache, &substituted).expect_err("refused");
    assert!(error.left_machine_unchanged());
    println!("  variant           refused: {error}");

    // 3. A different catalog. The release authenticates the catalog's digest.
    let mut substituted = honest.clone();
    substituted.catalog = Sha256Digest::from_bytes([9; 32]);
    let error =
        zup_windows::verify(&fixture.runtime, &fixture.cache, &substituted).expect_err("refused");
    assert!(error.left_machine_unchanged());
    println!("  catalog           refused: {error}");

    // 4. A different manifest. The variant descriptor names it.
    let mut substituted = honest.clone();
    substituted.manifest = Sha256Digest::from_bytes([8; 32]);
    let error =
        zup_windows::verify(&fixture.runtime, &fixture.cache, &substituted).expect_err("refused");
    assert!(error.left_machine_unchanged());
    println!("  manifest          refused: {error}");

    // 5. A different target. The release's variant descriptor carries it.
    let mut substituted = honest.clone();
    substituted.target =
        zup_core::TargetTriple::parse("aarch64-pc-windows-msvc").expect("a triple");
    let error =
        zup_windows::verify(&fixture.runtime, &fixture.cache, &substituted).expect_err("refused");
    assert!(error.left_machine_unchanged());
    println!("  target            refused: {error}");

    // 6. A different document, which is the one field a bootstrapper chooses
    //    freely. It is a cache key, and the document it selects has to be the
    //    release the handoff names.
    let mut substituted = honest.clone();
    substituted.document = digest_of(&fixture.manifest_bytes);
    let error =
        zup_windows::verify(&fixture.runtime, &fixture.cache, &substituted).expect_err("refused");
    assert!(error.left_machine_unchanged());
    println!("  document key      refused: {error}");

    // 7. A handoff that does not match the digest the launcher passed. Two
    //    processes disagreeing about what they handed over is a bug worth
    //    catching before anything is read.
    let tampered = write_handoff(&fixture, &honest);
    let error = zup_windows::accept_handoff(&tampered, Some(Sha256Digest::from_bytes([1; 32])))
        .expect_err("a handoff that does not match the launcher's digest is refused");
    println!("  document digest   refused: {error}");

    // And the honest one still verifies afterwards, so none of those refusals left
    // anything behind.
    zup_windows::verify(&fixture.runtime, &fixture.cache, &honest)
        .unwrap_or_else(|error| panic!("the honest handoff still verifies: {error}"));
    let _ = path;
}

#[test]
fn a_handoff_is_bounded_and_its_names_cannot_escape_a_directory() {
    // A handoff travels through a location a bootstrapper chose, so its names are
    // the one part of it that has to be treated as hostile before it is used.
    let fixture = fixture(2);
    let honest = handoff_for(&fixture);

    for (field, hostile) in [
        ("variant", "../../windows/system32"),
        ("scope", "machine/../user"),
    ] {
        let mut substituted = honest.clone();
        match field {
            "variant" => substituted.variant = hostile.to_owned(),
            _ => substituted.scope = hostile.to_owned(),
        }
        assert!(
            substituted.validate().is_err(),
            "{field} `{hostile}` must be refused before it reaches a path"
        );
    }

    // And the size bound is a real bound, not a comment: a document large enough
    // to be smuggling state rather than describing a handoff is refused unread.
    let oversized = vec![b'{'; (zup_acquire::MAX_HANDOFF_BYTES + 1) as usize];
    assert!(
        RuntimeHandoff::parse(&oversized).is_err(),
        "an oversized handoff is not a handoff"
    );

    // The honest one encodes to a few hundred bytes, which is the number a user
    // never sees and an operator can check.
    let bytes = honest.encode().expect("the handoff encodes");
    assert!(bytes.len() < 1024, "{} bytes", bytes.len());
    assert_eq!(
        RuntimeHandoff::parse(&bytes).expect("it parses").digest(),
        honest.digest()
    );
}
