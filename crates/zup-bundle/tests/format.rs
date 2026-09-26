use std::{fs, io::Read, path::Path};

use tempfile::TempDir;
use zup_build::TargetBuildPlan;
use zup_bundle::{
    AutoPayloadSource, BundleWriter, PACKAGE_SCHEMA, Package, PackageError, PayloadError,
    PayloadSource,
};
use zup_core::{RelativePath, hash_reader};
use zup_manifest::TargetOverrides;

fn plan(root: &Path) -> TargetBuildPlan {
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(root.join("dist/a.bin"), b"duplicate payload").unwrap();
    fs::write(root.join("dist/b.bin"), b"duplicate payload").unwrap();
    let manifest = format!(
        r#"
schema = 1
[app]
id = "com.example.bundle"
name = "Bundle Test"
version = "1.0.0"
[build]
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/BundleTest"
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
    let mut build =
        zup_build::materialize(&root.join("zup.toml"), &parsed, vec![(config, installer)]).unwrap();
    build.targets.pop().unwrap()
}

#[test]
fn package_create_read_and_write_round_trip_without_an_executable() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let package = Package::parse(&bytes).unwrap();
    assert_eq!(package.plan().entries.len(), 2);
    assert_eq!(package.build_plan().unwrap().targets.len(), 1);

    let package_path = root.path().join("package.zup");
    assert_eq!(
        BundleWriter::write_file(&plan(root.path()), &[], &package_path).unwrap(),
        bytes.len() as u64
    );
    assert_eq!(fs::read(&package_path).unwrap(), bytes);
    let opened = Package::open(&package_path).unwrap();
    assert_eq!(opened.plan(), package.plan());

    for name in ["a.bin", "b.bin"] {
        let path = RelativePath::new(name).unwrap();
        let entry = package
            .plan()
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        let mut reader = package
            .payload_source()
            .open(&path, &entry.sha256, entry.size)
            .unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"duplicate payload");
    }
}

#[test]
fn package_index_and_payload_source_are_public_package_data() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let package = Package::parse(&bytes).unwrap();
    let index = package.index_info();
    assert_eq!(
        index.index_size() as usize,
        60 + { u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize }
    );
    assert_eq!(index.blob_count(), 1);
    let index_bytes = package.index_bytes().unwrap();
    assert_eq!(Package::parse_index(&index_bytes).unwrap(), index);

    let path = RelativePath::new("a.bin").unwrap();
    let entry = package
        .plan()
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap();
    let mut reader = package
        .payload_source()
        .open(&path, &entry.sha256, entry.size)
        .unwrap();
    let mut data = Vec::new();
    reader.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"duplicate payload");
}

#[test]
fn package_tampering_is_rejected_before_payload_access() {
    let root = TempDir::new().unwrap();
    let mut bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 1;
    assert!(Package::parse(&bytes).is_err());
}

#[test]
fn package_verify_rechecks_file_contents() {
    let root = TempDir::new().unwrap();
    let package_path = root.path().join("package.zup");
    let mut bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    fs::write(&package_path, &bytes).unwrap();
    let package = Package::open(&package_path).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 1;
    fs::write(&package_path, bytes).unwrap();
    assert!(package.verify().is_err());
}

#[test]
fn package_auto_source_reads_a_standalone_package() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let package_path = root.path().join("package.zup");
    fs::write(&package_path, bytes).unwrap();
    let source = AutoPayloadSource::from_path(&package_path).unwrap();
    let path = RelativePath::new("a.bin").unwrap();
    let (size, digest) = hash_reader(&b"duplicate payload"[..]).unwrap();
    let mut reader = source.open(&path, &digest, size).unwrap();
    let mut data = Vec::new();
    reader.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"duplicate payload");
}

#[test]
fn overlay_precedes_base_and_missing_overlay_falls_back() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("base");
    let overlay = root.path().join("overlay");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&overlay).unwrap();
    fs::write(base.join("tool.bin"), b"base").unwrap();
    fs::write(overlay.join("tool.bin"), b"overlay").unwrap();
    let source = AutoPayloadSource::from_paths(&base, Some(overlay.clone())).unwrap();
    let relative = RelativePath::new("tool.bin").unwrap();
    let (size, digest) = hash_reader(&b"overlay"[..]).unwrap();
    let mut selected = Vec::new();
    source
        .open(&relative, &digest, size)
        .unwrap()
        .read_to_end(&mut selected)
        .unwrap();
    assert_eq!(selected, b"overlay");

    fs::remove_file(overlay.join("tool.bin")).unwrap();
    let (size, digest) = hash_reader(&b"base"[..]).unwrap();
    let mut fallback = Vec::new();
    source
        .open(&relative, &digest, size)
        .unwrap()
        .read_to_end(&mut fallback)
        .unwrap();
    assert_eq!(fallback, b"base");
}

#[test]
fn overlay_digest_mismatch_never_falls_back_to_base() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("base");
    let overlay = root.path().join("overlay");
    fs::create_dir_all(base.join("__zup_plugins__")).unwrap();
    fs::create_dir_all(overlay.join("__zup_plugins__")).unwrap();
    fs::write(base.join("__zup_plugins__/generated.bin"), b"good!").unwrap();
    fs::write(overlay.join("__zup_plugins__/generated.bin"), b"wrong").unwrap();
    let source = AutoPayloadSource::from_paths(base, Some(overlay)).unwrap();
    let relative = RelativePath::new("__zup_plugins__/generated.bin").unwrap();
    let (size, digest) = hash_reader(&b"good!"[..]).unwrap();
    assert!(matches!(
        source.open(&relative, &digest, size),
        Err(PayloadError::DigestMismatch { .. })
    ));
}

#[test]
fn reserved_payload_is_never_satisfied_by_a_directory_base() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("base");
    fs::create_dir_all(base.join("__zup_plugins__")).unwrap();
    fs::write(base.join("__zup_plugins__/generated.bin"), b"base").unwrap();
    let source = AutoPayloadSource::from_path(base).unwrap();
    let relative = RelativePath::new("__zup_plugins__/generated.bin").unwrap();
    let (size, digest) = hash_reader(&b"base"[..]).unwrap();
    assert!(matches!(
        source.open(&relative, &digest, size),
        Err(PayloadError::NotFound { .. })
    ));
}

#[test]
fn reserved_payload_root_does_not_case_fold() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("base");
    fs::create_dir_all(base.join("__ZUP_PLUGINS__")).unwrap();
    fs::write(
        base.join("__ZUP_PLUGINS__/generated.bin"),
        b"case-sensitive identity",
    )
    .unwrap();
    let source = AutoPayloadSource::from_path(base).unwrap();
    let relative = RelativePath::new("__ZUP_PLUGINS__/generated.bin").unwrap();
    let (size, digest) = hash_reader(&b"case-sensitive identity"[..]).unwrap();
    let mut reader = source.open(&relative, &digest, size).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"case-sensitive identity");
}

#[test]
fn malformed_package_headers_are_rejected() {
    assert!(matches!(
        Package::parse(b"not a package"),
        Err(PackageError::Io(_)) | Err(PackageError::Invalid)
    ));
    let root = TempDir::new().unwrap();
    let mut bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let mismatch = PACKAGE_SCHEMA + 1;
    bytes[8..12].copy_from_slice(&mismatch.to_le_bytes());
    assert!(matches!(Package::parse(bytes), Err(PackageError::Invalid)));
}

#[test]
fn oversized_metadata_is_rejected_from_the_header() {
    let mut bytes = vec![0u8; 60];
    bytes[..8].copy_from_slice(b"ZUPBNDL\0");
    bytes[8..12].copy_from_slice(&PACKAGE_SCHEMA.to_le_bytes());
    bytes[20..28].copy_from_slice(&(256u64 * 1024 * 1024 + 1).to_le_bytes());
    assert!(matches!(
        Package::parse(bytes),
        Err(PackageError::MetadataTooLarge { .. })
    ));
}
