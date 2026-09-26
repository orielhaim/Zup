//! The portable source-inspection policy and policy injection.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{
    BuildError, PortableSourceFilePolicy, Sha256Digest, SourceFilePolicy, materialize_with_policy,
};
use zup_core::Installer;
use zup_manifest::{Manifest, TargetOverrides, compile, parse, select_targets};

const PREREQUISITE: &[u8] = b"runtime payload";

/// A policy that records every path it is asked about, so a test can see that
/// prerequisite inspection consults the injected policy.
struct RecordingPolicy {
    /// Paths the policy reports as links, resolved without touching the disk.
    links: Vec<PathBuf>,
    asked: Mutex<Vec<PathBuf>>,
}

impl RecordingPolicy {
    /// A policy that reports `links` as links and nothing else as a link.
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

/// A policy whose inspection always fails, standing in for a host that cannot
/// read a source's ancestry.
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

/// A manifest with one embedded prerequisite at `path`, whose declared identity
/// matches `PREREQUISITE`.
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

/// A project whose embedded prerequisite is `vendor/runtime.exe`, with matching
/// content in `runtime.exe` so identity never explains a rejection.
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
    )
    .map_err(unwrap_target)?;
    assert_eq!(plan.targets.len(), 1);
    Ok(plan.targets.pop().unwrap())
}

fn materialize_default(
    dir: &Path,
    manifest: &Manifest,
) -> Result<zup_build::BuildPlan, BuildError> {
    zup_build::materialize(&dir.join("zup.toml"), manifest, vec![selection(manifest)])
}

fn unwrap_target(error: BuildError) -> BuildError {
    match error {
        BuildError::Target { source, .. } => *source,
        other => other,
    }
}

/// A symlink to `target`, or the `PermissionDenied` a host that withholds
/// `SeCreateSymbolicLinkPrivilege` answers with.
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

/// Whether this host can create a symlink at all. The Windows adapter's
/// junction test covers link refusal where it cannot.
fn can_symlink() -> bool {
    let dir = project(&[("target.bin", PREREQUISITE)]);
    symlink_to(&dir.path().join("target.bin"), &dir.path().join("link.bin")).is_ok()
}

// --- The portable policy ---

#[test]
fn portable_policy_reports_a_regular_file_and_a_missing_path_as_not_links() {
    let dir = project(&[("payload.exe", PREREQUISITE)]);

    assert!(
        !PortableSourceFilePolicy
            .is_link(&dir.path().join("payload.exe"))
            .unwrap(),
        "a regular file is not a link"
    );
    assert!(
        !PortableSourceFilePolicy
            .is_link(&dir.path().join("absent.exe"))
            .unwrap(),
        "a path that does not exist is not a link"
    );
}

#[test]
fn portable_policy_reports_a_symlink_as_a_link() {
    if !can_symlink() {
        return;
    }
    let dir = project(&[("payload.exe", PREREQUISITE)]);
    let link = dir.path().join("link.exe");
    symlink_to(&dir.path().join("payload.exe"), &link).unwrap();

    assert!(
        PortableSourceFilePolicy.is_link(&link).unwrap(),
        "a symlink is a link on every host, and the portable policy sees it"
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

    // The link resolves to content of exactly the declared size and digest, so
    // only the link check can reject it.
    let error = materialize_default(dir.path(), &manifest).unwrap_err();

    assert!(
        matches!(unwrap_target(error), BuildError::PrerequisiteSource { .. }),
        "materialize must inspect sources with the portable policy"
    );
}

// --- Injection ---

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

#[test]
fn an_injected_policy_decides_which_prerequisite_sources_are_usable() {
    let (dir, manifest) = prerequisite_project();
    let source = dir.path().join("vendor/runtime.exe");
    assert!(materialize_default(dir.path(), &manifest).is_ok());

    let error = prerequisite_target(
        dir.path(),
        &manifest,
        &RecordingPolicy::reporting(&[&source]),
    )
    .unwrap_err();

    assert!(
        matches!(error, BuildError::PrerequisiteSource { .. }),
        "{error:?}"
    );
}

#[test]
fn an_injected_policy_decides_which_ancestor_is_usable() {
    let (dir, manifest) = prerequisite_project();
    let ancestor = dir.path().join("vendor");

    let error = prerequisite_target(
        dir.path(),
        &manifest,
        &RecordingPolicy::reporting(&[&ancestor]),
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
