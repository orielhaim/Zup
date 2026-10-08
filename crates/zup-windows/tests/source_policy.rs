#![cfg(windows)]

use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{
    BuildError, PortableSourceFilePolicy, Sha256Digest, SourceFilePolicy, materialize_with_policy,
};
use zup_manifest::{Manifest, TargetOverrides, compile, parse, select_targets};
use zup_windows::WindowsSourceFilePolicy;

#[path = "fixture/junction.rs"]
mod junction;

use junction::Junction;

const PREREQUISITE: &[u8] = b"runtime payload";

fn manifest_with_prerequisite(path: &str) -> String {
    let digest = Sha256Digest::from_bytes(Sha256::digest(PREREQUISITE).into());
    format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = {{ directory = "dist" }}

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/Acme"

[[files]]
source = "**/*"
destination = "${{install}}"

[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = {{ kind = "runtime", id = "windows.vc.v14" }}
package = {{ type = "embedded", path = "{path}", sha256 = "{}", size = {} }}
"#,
        digest.to_hex(),
        PREREQUISITE.len()
    )
}

fn junction_project() -> (TempDir, Manifest, Junction) {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("dist")).unwrap();
    fs::write(dir.path().join("dist/app.exe"), b"app").unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("runtime.exe"), PREREQUISITE).unwrap();
    let junction = Junction::new(&dir.path().join("vendor"), &outside);
    let manifest = parse(&manifest_with_prerequisite("vendor/runtime.exe")).unwrap();
    (dir, manifest, junction)
}

fn materialize_target(
    dir: &Path,
    manifest: &Manifest,
    policy: &dyn SourceFilePolicy,
) -> Result<zup_build::TargetBuildPlan, BuildError> {
    let overrides = TargetOverrides::default();
    let config = select_targets(manifest, &["default"], &overrides)
        .unwrap()
        .remove(0);
    let installer = compile(manifest, &config, &overrides).unwrap();
    let mut plan = materialize_with_policy(
        &dir.join("zup.toml"),
        manifest,
        vec![(config, installer)],
        policy,
        zup_build::Writes::None,
    )
    .map_err(|error| match error {
        BuildError::Target { source, .. } => *source,
        other => other,
    })?;
    assert_eq!(plan.targets.len(), 1);
    Ok(plan.targets.pop().unwrap())
}

#[test]
fn windows_adapter_reports_a_junction_a_regular_file_and_a_missing_path() {
    let root = TempDir::new().unwrap();
    let regular = root.path().join("regular.exe");
    fs::write(&regular, PREREQUISITE).unwrap();
    let _junction = Junction::new(&root.path().join("linked"), &root.path().join("elsewhere"));

    assert!(!WindowsSourceFilePolicy.is_link(&regular).unwrap());
    assert!(
        !WindowsSourceFilePolicy
            .is_link(&root.path().join("missing.exe"))
            .unwrap(),
        "a path that does not exist is not a link"
    );
    assert!(
        WindowsSourceFilePolicy
            .is_link(&root.path().join("linked"))
            .unwrap(),
        "a directory junction carries FILE_ATTRIBUTE_REPARSE_POINT"
    );
}

#[test]
fn a_prerequisite_reached_through_a_junction_is_refused_by_every_policy() {
    for policy in [
        &WindowsSourceFilePolicy as &dyn SourceFilePolicy,
        &PortableSourceFilePolicy as &dyn SourceFilePolicy,
    ] {
        let (dir, manifest, _junction) = junction_project();
        let error = materialize_target(dir.path(), &manifest, policy).unwrap_err();
        assert!(
            matches!(error, BuildError::PrerequisiteSource { .. }),
            "{error:?}"
        );
    }
}

#[test]
fn a_source_inspection_failure_is_reported_rather_than_assumed_safe() {
    struct Unreadable;

    impl SourceFilePolicy for Unreadable {
        fn is_link(&self, _path: &Path) -> Result<bool, io::Error> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the source ancestry is unreadable",
            ))
        }
    }

    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("dist")).unwrap();
    fs::write(dir.path().join("dist/app.exe"), b"app").unwrap();
    fs::write(dir.path().join("runtime.exe"), PREREQUISITE).unwrap();
    let manifest = parse(&manifest_with_prerequisite("runtime.exe")).unwrap();

    let error = materialize_target(dir.path(), &manifest, &Unreadable).unwrap_err();

    assert!(matches!(error, BuildError::Io { .. }), "{error:?}");
}
