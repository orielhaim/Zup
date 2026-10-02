use std::{fs, path::Path};

use rstest::rstest;
use tempfile::TempDir;
use zup_binary::{Executable, ProgramKind};
use zup_build::TargetBuildPlan;
use zup_bundle::{BundleWriter, PayloadSource};
use zup_core::Frontend;
use zup_manifest::TargetOverrides;
use zup_windows::{
    BundleError, EmbeddedBundle, build_self_contained_executable, embed_bundle_file, read_frontend,
    validate_frontend,
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
    let mut build = zup_build::materialize(
        &root.join("zup.toml"),
        &parsed,
        vec![(config, installer)],
        zup_build::Writes::None,
    )
    .unwrap();
    build.targets.pop().unwrap()
}

/// Reading an image and deciding whether it may be the installer is one decision
/// at three levels: what the image declares, whether the bytes are an image at
/// all, and whether what they declare is the frontend the build asked for.
#[rstest]
#[case(pe_bytes(3), Some(Frontend::Console), true)]
#[case(pe_bytes(2), Some(Frontend::Gui), true)]
/// A driver records a subsystem that is neither a window nor a console. Its
/// machine type is still readable; its frontend is not guessed at.
#[case(pe_bytes(9), None, true)]
/// A file that is not an image at all.
#[case(b"not a PE".to_vec(), None, false)]
fn an_image_names_its_own_frontend_and_is_matched_against_the_requested_one(
    #[case] bytes: Vec<u8>,
    #[case] declared: Option<Frontend>,
    #[case] readable_target: bool,
) {
    let root = TempDir::new().unwrap();
    let runtime = root.path().join("runtime.exe");
    fs::write(&runtime, bytes).unwrap();
    let target =
        zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET).expect("the host target");

    let Some(frontend) = declared else {
        if !readable_target {
            // A file that is not an image at all: there is nothing to read, and
            // every question about it is refused at the read rather than answered.
            let error = read_frontend(&runtime).expect_err("not an image at all");
            assert!(matches!(error, BundleError::Inspect(_)), "{error}");
            assert!(
                Executable::read(&runtime).is_err(),
                "a file that is not an image states nothing about a target"
            );
            return;
        }
        // An image whose subsystem is neither a window nor a console. Its machine
        // type is still readable; its frontend is not guessed at.
        assert_eq!(
            read_frontend(&runtime).unwrap(),
            None,
            "a subsystem the loader has no frontend for is not guessed at"
        );
        assert!(
            Executable::read(&runtime).unwrap().matches_target(&target),
            "the machine type is still readable"
        );
        return;
    };

    assert_eq!(read_frontend(&runtime).unwrap(), Some(frontend));
    let executable = Executable::read(&runtime).unwrap();
    assert_eq!(
        executable.program(),
        Some(match frontend {
            Frontend::Gui => ProgramKind::Windowed,
            _ => ProgramKind::Console,
        })
    );
    assert!(executable.matches_target(&target));
    assert!(validate_frontend(&runtime, frontend).is_ok());

    let other = match frontend {
        Frontend::Gui => Frontend::Console,
        Frontend::Console | Frontend::Headless => Frontend::Gui,
    };
    assert!(
        validate_frontend(&runtime, other).is_err(),
        "a {frontend:?} image may not launch a {other:?} installer"
    );
    // A headless frontend is a console program that also promises to speak a
    // protocol, so a console image serves it and a windowed one does not.
    assert_eq!(
        validate_frontend(&runtime, Frontend::Headless).is_ok(),
        frontend == Frontend::Console
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
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package, None).unwrap();
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
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package, None).unwrap();
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
    // Whatever this test binary happens to be, so the round trip exercises the
    // matching case rather than a deliberate mismatch.
    build.installer.frontend = read_frontend(&std::env::current_exe().unwrap())
        .unwrap()
        .expect("a test binary on Windows records a subsystem");
    let output = root.path().join("Matching.exe");
    let (_, package_size) = build_self_contained_executable(
        &std::env::current_exe().unwrap(),
        &output,
        &build,
        &[],
        None,
    )
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
    embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package, None).unwrap();
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
