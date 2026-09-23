use std::{fs, io::Read, path::Path};

use tempfile::TempDir;
use zup_bundle::{BundleWriter, EmbeddedBundle, PayloadSource, embed_bundle_file};
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
    let bytes = BundleWriter::encode(&plan(root.path())).unwrap();
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
fn resource_bundle_corruption_is_rejected_before_payload_use() {
    let root = TempDir::new().unwrap();
    let mut bytes = BundleWriter::encode(&plan(root.path())).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 0x40;
    let output = embed(root.path(), &bytes);
    assert!(EmbeddedBundle::open(output).is_err());
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
