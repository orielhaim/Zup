use std::{fs, io::Read, path::Path};

use tempfile::TempDir;
#[cfg(windows)]
use zup_bundle::build_self_contained_executable;
use zup_bundle::{
    AutoPayloadSource, BundleError, BundleWriter, EmbeddedBundle, PayloadError, PayloadSource,
    embed_bundle_file, read_pe_frontend, read_pe_target, validate_pe_frontend,
};
use zup_core::{Frontend, RelativePath};

#[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn pe_target_reads_the_current_test_executable() {
    assert_eq!(
        read_pe_target(&std::env::current_exe().unwrap()).unwrap(),
        zup_plugin_contract::HOST_TARGET,
    );
}

#[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn pe_frontend_reads_the_current_cui_test_executable() {
    assert_eq!(
        read_pe_frontend(&std::env::current_exe().unwrap()).unwrap(),
        Frontend::Console
    );
}

#[cfg(windows)]
#[test]
fn executable_without_bundle_resource_is_classified_as_missing() {
    let error = match EmbeddedBundle::open(std::env::current_exe().unwrap()) {
        Ok(_) => panic!("test executable unexpectedly contains a bundle"),
        Err(error) => error,
    };
    assert!(error.is_missing_resource(), "{error}");
}

#[test]
fn pe_target_rejects_non_pe_bytes() {
    let root = TempDir::new().unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, b"not a PE").unwrap();
    assert!(matches!(
        read_pe_target(&runtime),
        Err(BundleError::Invalid)
    ));
}

fn pe_bytes(subsystem: u16) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x170];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
    bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
    bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
    bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
    bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
    bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
    bytes
}

#[test]
fn pe_frontend_reads_cui_and_gui_subsystems() {
    let root = TempDir::new().unwrap();
    let cui = root.path().join("cui.exe");
    let gui = root.path().join("gui.exe");
    fs::write(&cui, pe_bytes(3)).unwrap();
    fs::write(&gui, pe_bytes(2)).unwrap();
    assert_eq!(read_pe_frontend(&cui).unwrap(), Frontend::Console);
    assert_eq!(read_pe_frontend(&gui).unwrap(), Frontend::Gui);
}

#[test]
fn pe_frontend_rejects_unknown_subsystems() {
    let root = TempDir::new().unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, pe_bytes(9)).unwrap();
    assert!(matches!(
        read_pe_frontend(&runtime),
        Err(BundleError::Invalid)
    ));
}

#[test]
fn pe_frontend_validation_rejects_mismatches() {
    let root = TempDir::new().unwrap();
    let cui = root.path().join("cui.exe");
    fs::write(&cui, pe_bytes(3)).unwrap();
    assert!(validate_pe_frontend(&cui, Frontend::Console).is_ok());
    assert!(matches!(
        validate_pe_frontend(&cui, Frontend::Gui),
        Err(BundleError::FrontendMismatch {
            expected: Frontend::Gui,
            found: Frontend::Console,
        })
    ));
}

fn plan(root: &Path) -> zup_build::BuildPlan {
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(root.join("dist/a.bin"), b"duplicate payload").unwrap();
    fs::write(root.join("dist/b.bin"), b"duplicate payload").unwrap();
    let manifest = r#"
schema = 1
[app]
id = "com.example.bundle"
name = "Bundle Test"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/BundleTest"
[[files]]
source = "**/*"
destination = "${install}"
"#;
    let parsed = zup_manifest::parse(manifest).unwrap();
    let installer = zup_manifest::parse_and_compile(manifest).unwrap();
    zup_build::materialize(&root.join("zup.toml"), &parsed, installer).unwrap()
}

fn embed(root: &Path, package: &[u8]) -> std::path::PathBuf {
    let package_path = root.join("bundle.zupbundle");
    fs::write(&package_path, package).unwrap();
    let output = root.join("Setup.exe");
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path).unwrap();
    output
}

#[test]
fn package_is_inside_an_authenticode_hashed_pe_resource_and_deduplicated() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let output = embed(root.path(), &bytes);
    let embedded = EmbeddedBundle::open(&output).unwrap();
    let (exe_size, exe_hash) = zup_core::hash_reader(fs::File::open(&output).unwrap()).unwrap();
    let mut maintenance = embedded
        .payload_source()
        .open(
            &RelativePath::new("__zup_maintenance__.exe").unwrap(),
            &exe_hash,
            exe_size,
        )
        .unwrap();
    let mut copy = Vec::new();
    maintenance.read_to_end(&mut copy).unwrap();
    assert_eq!(copy, fs::read(&output).unwrap());
    assert_eq!(embedded.plan().entries.len(), 2);
    let payload = embedded.payload_source();
    for name in ["a.bin", "b.bin"] {
        let path = RelativePath::new(name).unwrap();
        let entry = embedded
            .plan()
            .entries
            .iter()
            .find(|e| e.path == path)
            .unwrap();
        let mut reader = payload.open(&path, &entry.sha256, entry.size).unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"duplicate payload");
    }
    let meta_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    let meta: serde_json::Value = serde_json::from_slice(&bytes[60..60 + meta_len]).unwrap();
    assert_eq!(meta["blobs"].as_array().unwrap().len(), 1);
    assert_eq!(
        pe_section_end(&output),
        fs::metadata(&output).unwrap().len(),
        "package must not be PE overlay data"
    );
}

#[test]
fn bundle_records_the_selected_frontend() {
    let root = TempDir::new().unwrap();
    let mut build = plan(root.path());
    build.installer.frontend = Frontend::Console;
    let bytes = BundleWriter::encode(&build, &[]).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    let metadata: serde_json::Value =
        serde_json::from_slice(&bytes[60..60 + metadata_len]).unwrap();
    assert_eq!(metadata["plan"]["installer"]["frontend"], "console");
}

#[cfg(windows)]
#[test]
fn self_contained_build_rejects_frontend_mismatch() {
    let root = TempDir::new().unwrap();
    let mut build = plan(root.path());
    let found = read_pe_frontend(&std::env::current_exe().unwrap()).unwrap();
    let expected = match found {
        Frontend::Console => Frontend::Gui,
        Frontend::Gui => Frontend::Console,
        Frontend::Headless => Frontend::Gui,
    };
    build.installer.frontend = expected;
    let error = build_self_contained_executable(
        &std::env::current_exe().unwrap(),
        &root.path().join("Setup.exe"),
        &build,
        &[],
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BundleError::FrontendMismatch {
            expected: actual_expected,
            found: actual_found,
        } if actual_expected == expected && actual_found == found
    ));
}

#[test]
fn resource_bundle_corruption_is_rejected_before_payload_use() {
    let root = TempDir::new().unwrap();
    let mut bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 0x40;
    let output = embed(root.path(), &bytes);
    assert!(EmbeddedBundle::open(output).is_err());
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
    let (size, digest) = zup_core::hash_reader(&b"overlay"[..]).unwrap();
    let mut selected = Vec::new();
    source
        .open(&relative, &digest, size)
        .unwrap()
        .read_to_end(&mut selected)
        .unwrap();
    assert_eq!(selected, b"overlay");

    fs::remove_file(overlay.join("tool.bin")).unwrap();
    let (size, digest) = zup_core::hash_reader(&b"base"[..]).unwrap();
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
    let (size, digest) = zup_core::hash_reader(&b"good!"[..]).unwrap();
    assert!(matches!(
        source.open(&relative, &digest, size),
        Err(PayloadError::DigestMismatch { .. })
    ));
}

#[test]
fn reserved_payload_is_never_satisfied_by_the_base_source() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("base");
    fs::create_dir_all(base.join("__zup_plugins__")).unwrap();
    fs::write(base.join("__zup_plugins__/generated.bin"), b"base").unwrap();
    let source = AutoPayloadSource::from_path(base).unwrap();
    let relative = RelativePath::new("__zup_plugins__/generated.bin").unwrap();
    let (size, digest) = zup_core::hash_reader(&b"base"[..]).unwrap();
    assert!(matches!(
        source.open(&relative, &digest, size),
        Err(PayloadError::NotFound { .. })
    ));
}

fn pe_section_end(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..2], b"MZ");
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
    let section_count = u16::from_le_bytes(bytes[pe + 6..pe + 8].try_into().unwrap()) as usize;
    let optional_size = u16::from_le_bytes(bytes[pe + 20..pe + 22].try_into().unwrap()) as usize;
    let sections = pe + 24 + optional_size;
    (0..section_count)
        .map(|index| {
            let section = sections + index * 40;
            let raw_size =
                u32::from_le_bytes(bytes[section + 16..section + 20].try_into().unwrap()) as u64;
            let raw_offset =
                u32::from_le_bytes(bytes[section + 20..section + 24].try_into().unwrap()) as u64;
            raw_offset + raw_size
        })
        .max()
        .unwrap_or(0)
}
