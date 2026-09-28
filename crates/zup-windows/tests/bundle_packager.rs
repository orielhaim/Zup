use std::{fs, path::Path};

use rstest::rstest;
use tempfile::TempDir;
use zup_build::TargetBuildPlan;
use zup_bundle::{BundleWriter, PayloadSource};
use zup_core::Frontend;
use zup_manifest::TargetOverrides;
use zup_windows::{
    BundleError, EmbeddedBundle, PeSubsystem, build_self_contained_executable, embed_bundle_file,
    read_pe_frontend, read_pe_subsystem, read_pe_target, validate_pe_frontend,
};

fn plan(root: &Path) -> TargetBuildPlan {
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(root.join("dist/a.bin"), b"payload").unwrap();
    let manifest = format!(
        r#"
schema = 1
[app]
id = "com.example.windows-bundle"
name = "Windows Bundle"
version = "1.0.0"
[build]
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/WindowsBundle"
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

/// Reading a PE and deciding whether it may be the installer is one decision at
/// three levels: what the image declares, whether the bytes are an image at all,
/// and whether what they declare is the frontend the build asked for. The three
/// share one parser, so they are one table.
#[rstest]
#[case(pe_bytes(3), Some((PeSubsystem::Console, Frontend::Console)), true)]
#[case(pe_bytes(2), Some((PeSubsystem::Gui, Frontend::Gui)), true)]
#[case(pe_bytes(9), None, true)]
#[case(b"not a PE".to_vec(), None, false)]
fn a_pe_image_names_its_own_frontend_and_is_matched_against_the_requested_one(
    #[case] bytes: Vec<u8>,
    #[case] declared: Option<(PeSubsystem, Frontend)>,
    #[case] readable_target: bool,
) {
    let root = TempDir::new().unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, bytes).unwrap();

    let Some((subsystem, frontend)) = declared else {
        assert!(
            matches!(read_pe_frontend(&runtime), Err(BundleError::Invalid)),
            "a subsystem the loader has no frontend for is not guessed at"
        );
        if readable_target {
            assert!(
                read_pe_target(&runtime).is_ok(),
                "the machine type is still readable"
            );
        } else {
            assert!(matches!(
                read_pe_target(&runtime),
                Err(BundleError::Invalid)
            ));
        }
        return;
    };

    assert_eq!(read_pe_subsystem(&runtime).unwrap(), subsystem);
    assert_eq!(read_pe_frontend(&runtime).unwrap(), frontend);
    assert!(read_pe_target(&runtime).is_ok());
    assert_eq!(
        read_pe_target(&runtime).unwrap(),
        zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    );
    assert!(validate_pe_frontend(&runtime, frontend).is_ok());
    let other = match frontend {
        Frontend::Console => Frontend::Gui,
        Frontend::Gui => Frontend::Console,
        Frontend::Headless => Frontend::Console,
    };
    assert!(
        matches!(
            validate_pe_frontend(&runtime, other),
            Err(BundleError::FrontendMismatch { .. })
        ),
        "a {frontend:?} image may not launch a {other:?} installer"
    );
}

#[cfg(windows)]
#[test]
fn embed_and_open_round_trip_a_real_package() {
    let root = TempDir::new().unwrap();
    let package = root.path().join("package.zup");
    fs::write(
        &package,
        BundleWriter::encode(&plan(root.path()), &[]).unwrap(),
    )
    .unwrap();
    let output = root.path().join("Setup.exe");
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package).unwrap();
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.plan().entries.len(), 1);
    assert_eq!(bundle.frontend(), bundle.plan().installer.frontend);
}

#[cfg(windows)]
#[test]
fn embedded_package_is_owned_after_the_executable_is_removed() {
    let root = TempDir::new().unwrap();
    let package = root.path().join("package.zup");
    fs::write(
        &package,
        BundleWriter::encode(&plan(root.path()), &[]).unwrap(),
    )
    .unwrap();
    let output = root.path().join("Setup.exe");
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package).unwrap();
    let bundle = EmbeddedBundle::open(&output).unwrap();
    let package = bundle.package().clone();
    fs::remove_file(&output).unwrap();
    assert_eq!(package.plan().entries.len(), 1);
    let entry = &package.plan().entries[0];
    let mut reader = package
        .payload_source()
        .open(&entry.path, &entry.sha256, entry.size)
        .unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
    assert_eq!(bytes, b"payload");
}

#[cfg(windows)]
#[test]
fn embedded_reader_reports_a_missing_index() {
    let error = EmbeddedBundle::open(std::env::current_exe().unwrap()).unwrap_err();
    assert!(error.is_missing_resource(), "{error}");
}

#[cfg(windows)]
#[test]
fn self_contained_build_round_trips_matching_target() {
    let root = TempDir::new().unwrap();
    let mut build = plan(root.path());
    build.installer.frontend = read_pe_frontend(&std::env::current_exe().unwrap()).unwrap();
    let output = root.path().join("Matching.exe");
    let (_, package_size) =
        build_self_contained_executable(&std::env::current_exe().unwrap(), &output, &build, &[])
            .unwrap();
    assert!(package_size > 0);
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.plan().installer.target, build.installer.target);
}

#[cfg(windows)]
#[test]
fn embed_accepts_structure_and_open_rejects_a_tampered_blob() {
    let root = TempDir::new().unwrap();
    let mut bytes = BundleWriter::encode(&plan(root.path()), &[]).unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 1;
    let package = root.path().join("package.zup");
    fs::write(&package, bytes).unwrap();
    let output = root.path().join("Setup.exe");
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package).unwrap();
    assert!(output.exists());
    assert!(EmbeddedBundle::open(&output).is_err());
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
