use std::{fs, io::Read, path::Path};

use tempfile::TempDir;
use zup_bundle::{BundleWriter, EmbeddedBundle, PayloadSource, append_bundle_to_executable};
use zup_core::RelativePath;

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
    let manifest_path = root.join("zup.toml");
    zup_build::materialize(&manifest_path, &parsed, installer).unwrap()
}

#[test]
fn embedded_bundle_is_deterministic_deduplicated_and_random_accessible() {
    let root = TempDir::new().unwrap();
    let build = plan(root.path());
    let first = BundleWriter::encode(&build).unwrap();
    let second = BundleWriter::encode(&build).unwrap();
    assert_eq!(first, second);
    let streamed = root.path().join("streamed.zupbundle");
    BundleWriter::write_file(&build, &streamed).unwrap();
    assert_eq!(fs::read(&streamed).unwrap(), first);
    let runtime = root.path().join("runtime.exe");
    let output = root.path().join("setup.exe");
    fs::write(&runtime, b"MZ test runtime").unwrap();
    append_bundle_to_executable(&runtime, &output, &first).unwrap();
    let embedded = EmbeddedBundle::open(&output).unwrap();
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
    let meta_len = u64::from_le_bytes(first[20..28].try_into().unwrap()) as usize;
    let meta: serde_json::Value = serde_json::from_slice(&first[60..60 + meta_len]).unwrap();
    assert_eq!(meta["blobs"].as_array().unwrap().len(), 1);
    assert!(!String::from_utf8_lossy(&first).contains(root.path().to_str().unwrap()));
}

#[test]
fn corruption_in_bundle_or_footer_is_rejected_before_payload_use() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, b"MZ runtime").unwrap();
    let output = root.path().join("setup.exe");
    append_bundle_to_executable(&runtime, &output, &bytes).unwrap();
    let mut file = fs::read(&output).unwrap();
    let last = file.len() - 1;
    file[last] ^= 0x80;
    fs::write(&output, file).unwrap();
    assert!(EmbeddedBundle::open(&output).is_err());
}

#[test]
fn payload_blob_tampering_is_rejected_even_when_locator_checksum_is_rebuilt() {
    use sha2::{Digest, Sha256};

    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let runtime = root.path().join("runtime.exe");
    let output = root.path().join("setup.exe");
    fs::write(&runtime, b"MZ runtime").unwrap();
    append_bundle_to_executable(&runtime, &output, &bytes).unwrap();
    let mut artifact = fs::read(&output).unwrap();
    let package_start = fs::metadata(&runtime).unwrap().len() as usize;
    let metadata_size = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    let payload_start = package_start + 60 + metadata_size;
    artifact[payload_start] ^= 0x01;
    let package_end = package_start + bytes.len();
    let package_hash = Sha256::digest(&artifact[package_start..package_end]);
    artifact[package_end + 32..package_end + 64].copy_from_slice(&package_hash);
    fs::write(&output, artifact).unwrap();
    assert!(EmbeddedBundle::open(&output).is_err());
}

#[test]
fn out_of_bounds_index_offset_is_rejected_after_container_checksums_are_rebuilt() {
    use sha2::{Digest, Sha256};

    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let runtime = root.path().join("runtime.exe");
    let output = root.path().join("setup.exe");
    fs::write(&runtime, b"MZ runtime").unwrap();
    append_bundle_to_executable(&runtime, &output, &bytes).unwrap();
    let mut artifact = fs::read(&output).unwrap();
    let package_start = fs::metadata(&runtime).unwrap().len() as usize;
    let metadata_size = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    let metadata_start = package_start + 60;
    let metadata_end = metadata_start + metadata_size;
    let marker = b"\"offset\":0";
    let offset = artifact[metadata_start..metadata_end]
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    artifact[metadata_start + offset + marker.len() - 1] = b'9';
    let metadata_hash = Sha256::digest(&artifact[metadata_start..metadata_end]);
    artifact[package_start + 28..package_start + 60].copy_from_slice(&metadata_hash);
    let package_end = package_start + bytes.len();
    let package_hash = Sha256::digest(&artifact[package_start..package_end]);
    artifact[package_end + 32..package_end + 64].copy_from_slice(&package_hash);
    fs::write(&output, artifact).unwrap();
    assert!(EmbeddedBundle::open(&output).is_err());
}

#[test]
fn truncated_locator_is_rejected() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, b"MZ runtime").unwrap();
    let output = root.path().join("setup.exe");
    append_bundle_to_executable(&runtime, &output, &bytes).unwrap();
    let mut file = fs::read(&output).unwrap();
    file.truncate(file.len() - 5);
    fs::write(&output, file).unwrap();
    assert!(EmbeddedBundle::open(&output).is_err());
}

#[test]
fn locator_survives_a_certificate_table_appended_after_the_bundle() {
    let root = TempDir::new().unwrap();
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let runtime = root.path().join("runtime.exe");
    let output = root.path().join("setup.exe");
    let mut pe = vec![0u8; 0x80 + 24 + 224];
    pe[..2].copy_from_slice(b"MZ");
    pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    pe[0x94..0x96].copy_from_slice(&224u16.to_le_bytes());
    let optional = 0x80 + 24;
    pe[optional..optional + 2].copy_from_slice(&0x10bu16.to_le_bytes());
    fs::write(&runtime, pe).unwrap();
    append_bundle_to_executable(&runtime, &output, &bytes).unwrap();
    let mut signed = fs::read(&output).unwrap();
    let certificate_offset = signed.len() as u32;
    let security_directory = 0x80 + 24 + 96 + 4 * 8;
    signed[security_directory..security_directory + 4]
        .copy_from_slice(&certificate_offset.to_le_bytes());
    signed[security_directory + 4..security_directory + 8].copy_from_slice(&8u32.to_le_bytes());
    signed.extend_from_slice(&[0x30, 6, 1, 1, 0, 0, 0, 0]);
    fs::write(&output, signed).unwrap();
    assert_eq!(
        EmbeddedBundle::open(&output).unwrap().plan().entries.len(),
        2
    );
}
