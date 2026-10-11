use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rstest::rstest;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{
    BuildError, PortableSourceFilePolicy, Sha256Digest, SourceFilePolicy, materialize_with_policy,
};
use zup_core::Installer;
use zup_manifest::{Manifest, TargetOverrides, compile, parse, select_targets};

const PREREQUISITE: &[u8] = b"runtime payload";

struct RecordingPolicy {
    links: Vec<PathBuf>,
    asked: Mutex<Vec<PathBuf>>,
}

impl RecordingPolicy {
    fn reporting(links: &[&Path]) -> Self {
        Self {
            links: links.iter().map(|path| path.to_path_buf()).collect(),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<PathBuf> {
        self.asked.lock().unwrap().clone()
    }
}

impl SourceFilePolicy for RecordingPolicy {
    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        self.asked.lock().unwrap().push(path.to_path_buf());
        Ok(self.links.iter().any(|link| link == path))
    }
}

struct Unreadable;

impl SourceFilePolicy for Unreadable {
    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is unreadable", path.display()),
        ))
    }
}

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn project(files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (relative, contents) in files {
        write(&dir.path().join(relative), contents);
    }
    dir
}

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

fn prerequisite_project() -> (TempDir, Manifest) {
    let dir = project(&[
        ("dist/app.exe", b"app"),
        ("runtime.exe", PREREQUISITE),
        ("vendor/runtime.exe", PREREQUISITE),
    ]);
    let manifest = parse(&manifest_with_prerequisite("vendor/runtime.exe")).unwrap();
    (dir, manifest)
}

fn selection(manifest: &Manifest) -> (zup_core::ResolvedTargetConfig, Installer) {
    let overrides = TargetOverrides::default();
    let config = select_targets(manifest, &["default"], &overrides)
        .unwrap()
        .remove(0);
    let installer = compile(manifest, &config, &overrides).unwrap();
    (config, installer)
}

fn prerequisite_target(
    dir: &Path,
    manifest: &Manifest,
    policy: &dyn SourceFilePolicy,
) -> Result<zup_build::TargetBuildPlan, BuildError> {
    let mut plan = materialize_with_policy(
        &dir.join("zup.toml"),
        manifest,
        vec![selection(manifest)],
        policy,
        zup_build::Writes::None,
    )
    .map_err(unwrap_target)?;
    assert_eq!(plan.targets.len(), 1);
    Ok(plan.targets.pop().unwrap())
}

fn materialize_default(
    dir: &Path,
    manifest: &Manifest,
) -> Result<zup_build::BuildPlan, BuildError> {
    zup_build::materialize(
        &dir.join("zup.toml"),
        manifest,
        vec![selection(manifest)],
        zup_build::Writes::None,
    )
}

fn unwrap_target(error: BuildError) -> BuildError {
    match error {
        BuildError::Target { source, .. } => *source,
        other => other,
    }
}

fn symlink_to(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

fn can_symlink() -> bool {
    let dir = project(&[("target.bin", PREREQUISITE)]);
    symlink_to(&dir.path().join("target.bin"), &dir.path().join("link.bin")).is_ok()
}

#[rstest]
#[case::a_regular_file("payload.exe", false, false)]
#[case::a_path_that_does_not_exist("absent.exe", false, false)]
#[case::a_symlink("link.exe", true, true)]
fn the_portable_policy_calls_only_a_symlink_a_link(
    #[case] name: &str,
    #[case] create: bool,
    #[case] is_link: bool,
) {
    let dir = project(&[("payload.exe", PREREQUISITE)]);
    let path = dir.path().join(name);
    if create {
        if !can_symlink() {
            return;
        }
        symlink_to(&dir.path().join("payload.exe"), &path).unwrap();
    }
    assert_eq!(
        PortableSourceFilePolicy.is_link(&path).unwrap(),
        is_link,
        "{name}"
    );
}

#[test]
fn materialize_defaults_to_the_portable_policy() {
    if !can_symlink() {
        return;
    }
    let (dir, manifest) = prerequisite_project();
    let source = dir.path().join("vendor/runtime.exe");
    let target = dir.path().join("runtime.exe");
    fs::remove_file(&source).unwrap();
    symlink_to(&target, &source).unwrap();

    let error = materialize_default(dir.path(), &manifest).unwrap_err();

    assert!(
        matches!(unwrap_target(error), BuildError::PrerequisiteSource { .. }),
        "materialize must inspect sources with the portable policy"
    );
}

#[test]
fn every_prerequisite_source_and_ancestor_is_inspected_through_the_injected_policy() {
    let (dir, manifest) = prerequisite_project();
    let source = dir.path().join("vendor/runtime.exe");
    let policy = RecordingPolicy::reporting(&[]);

    prerequisite_target(dir.path(), &manifest, &policy).unwrap();

    let asked = policy.asked();
    assert_eq!(
        asked.first(),
        Some(&source),
        "the source itself is inspected first"
    );
    assert!(
        asked
            .windows(2)
            .any(|pair| pair == [&source, source.parent().unwrap()]),
        "the directory holding the source is inspected next: {asked:?}"
    );
    assert!(
        asked.iter().any(|path| path == dir.path()),
        "inspection walks past the project directory, not just the source's own: {asked:?}"
    );
}

#[rstest]
#[case::the_source_itself("vendor/runtime.exe")]
#[case::a_directory_holding_it("vendor")]
fn an_injected_policy_decides_which_prerequisite_path_is_usable(#[case] relative: &str) {
    let (dir, manifest) = prerequisite_project();
    let linked = dir.path().join(relative);

    assert!(materialize_default(dir.path(), &manifest).is_ok());
    let error = prerequisite_target(
        dir.path(),
        &manifest,
        &RecordingPolicy::reporting(&[&linked]),
    )
    .unwrap_err();

    assert!(
        matches!(error, BuildError::PrerequisiteSource { .. }),
        "{error:?}"
    );
}

#[test]
fn a_source_inspection_failure_is_reported_rather_than_assumed_safe() {
    let (dir, manifest) = prerequisite_project();

    let error = prerequisite_target(dir.path(), &manifest, &Unreadable).unwrap_err();

    assert!(matches!(error, BuildError::Io { .. }), "{error:?}");
}
