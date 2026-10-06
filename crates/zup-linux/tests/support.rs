//! Shared Linux lifecycle test fixtures: isolated users, fixture packages, and
//! composed installers.
//!
//! One module because the E2E tests share one story: an isolated home, a tiny
//! application with a script that reports its version, a real package, and a
//! real installer composed from a template. Duplicating that across test files
//! would let the fixtures drift apart until two tests disagreed about what an
//! application is.
//!
//! Each integration test target compiles its own copy of this module and uses
//! a different subset, so unused helpers are normal rather than dead code.
//! The allow keeps one target's subset from failing another's build.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use zup_bundle::BundleWriter;
use zup_core::{
    App, AppId, Frontend, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    RelativePath, ResolvedFile, SelectedScope, TargetBuildPlan, TargetTriple, Template,
    hash_reader,
};
use zup_linux::{LinuxAction, LinuxOutcome, LinuxRunRequest, run};

/// Process-global environment variables are process-global: two tests
/// mutating `HOME` at once would resolve each other's locations. The E2E
/// tests hold this across the whole isolated run instead.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// An isolated Linux user: temporary home and XDG directories, installed as
/// the process environment and restored on drop.
pub struct IsolatedUser {
    _lock: MutexGuard<'static, ()>,
    prior: Vec<(String, Option<std::ffi::OsString>)>,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    pub state: PathBuf,
}

impl IsolatedUser {
    pub fn isolate() -> Self {
        // A poisoned lock means a previous test panicked mid-run: the
        // environment may be half-set, so re-establishing it unconditionally
        // below is what makes the next test isolated rather than inheriting
        // the wreckage.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().expect("a temp directory");
        let home = root.path().join("home");
        let mut prior = Vec::new();
        let dirs = [
            ("HOME", home.clone()),
            ("XDG_DATA_HOME", home.join(".local/share")),
            ("XDG_STATE_HOME", home.join(".local/state")),
            ("XDG_CONFIG_HOME", home.join(".config")),
            ("XDG_CACHE_HOME", home.join(".cache")),
            ("XDG_RUNTIME_DIR", root.path().join("run")),
        ];
        for (name, path) in dirs {
            std::fs::create_dir_all(&path).expect("an isolated directory");
            prior.push((name.to_owned(), std::env::var_os(name)));
            // SAFETY: the lock above serializes every environment mutation in
            // this test binary, so no other thread observes a half-set home.
            unsafe { std::env::set_var(name, &path) };
        }
        let state = root.path().join("state-root");
        Self {
            _lock: lock,
            prior,
            root,
            state,
        }
    }

    /// Where this user's programs install to.
    pub fn programs(&self) -> PathBuf {
        let home = std::env::var_os("HOME").expect("HOME is isolated");
        PathBuf::from(home).join(".local/lib/zup/apps")
    }

    /// The environment as explicit child-process variables, so a spawned
    /// installer resolves the same isolated home without the parent mutating
    /// anything.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let home = std::env::var_os("HOME").expect("HOME is isolated");
        let home = PathBuf::from(home);
        let var =
            |name: &str, path: PathBuf| (name.to_owned(), path.to_string_lossy().into_owned());
        vec![
            var("HOME", home.clone()),
            var("XDG_DATA_HOME", home.join(".local/share")),
            var("XDG_STATE_HOME", home.join(".local/state")),
            var("XDG_CONFIG_HOME", home.join(".config")),
            var("XDG_CACHE_HOME", home.join(".cache")),
            var("XDG_RUNTIME_DIR", self.root.path().join("run")),
        ]
    }
}

impl Drop for IsolatedUser {
    fn drop(&mut self) {
        for (name, value) in self.prior.drain(..) {
            match value {
                // SAFETY: same serialization as the setup above.
                Some(previous) => unsafe { std::env::set_var(&name, previous) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }
}

/// One payload file in a fixture application.
pub struct FixtureFile {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

pub fn tool_script(version: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"tool {version}\"; exit 0; fi\nif [ \"$1\" = \"read-payload\" ]; then cat \"$(dirname \"$0\")/keep.dat\"; exit 0; fi\necho \"tool: unknown command $1\" >&2\nexit 1\n"
    )
    .into_bytes()
}

pub fn v1_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("1.0.0"),
            executable: true,
        },
        FixtureFile {
            name: "keep.dat",
            bytes: b"keep-v1".to_vec(),
            executable: false,
        },
        FixtureFile {
            name: "removed-in-v2.dat",
            bytes: b"doomed".to_vec(),
            executable: false,
        },
    ]
}

pub fn v2_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("1.1.0"),
            executable: true,
        },
        FixtureFile {
            name: "keep.dat",
            bytes: b"keep-v1".to_vec(),
            executable: false,
        },
        FixtureFile {
            name: "added-in-v2.dat",
            bytes: b"new".to_vec(),
            executable: false,
        },
    ]
}

/// Build a real package for one fixture application version.
pub fn package_bytes(scratch: &Path, version: &str, files: &[FixtureFile]) -> Vec<u8> {
    package_bytes_for(
        scratch,
        &TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target"),
        version,
        files,
    )
}

/// Build a real package for an explicit target triple.
///
/// The target is a parameter rather than a constant so a mismatch test can
/// pair a genuine Linux runtime with a package for another platform and prove
/// the carrier refuses the pairing before anything is mutated.
pub fn package_bytes_for(
    scratch: &Path,
    target: &TargetTriple,
    version: &str,
    files: &[FixtureFile],
) -> Vec<u8> {
    let target = target.clone();
    let payload_dir = scratch.join("payload");
    std::fs::create_dir_all(&payload_dir).expect("a payload directory");
    let resolved = files
        .iter()
        .map(|file| {
            let source = payload_dir.join(file.name);
            std::fs::write(&source, &file.bytes).expect("a payload file");
            let (size, sha256) = hash_reader(file.bytes.as_slice()).expect("a payload hashes");
            ResolvedFile {
                source,
                source_relative: RelativePath::new(file.name).expect("a relative path"),
                destination: Template::parse(&format!("${{location.programs}}/tool/{}", file.name))
                    .expect("a destination"),
                size,
                sha256,
                component: None,
                condition: None,
                executable: file.executable,
            }
        })
        .collect::<Vec<_>>();
    let total_size = resolved.iter().map(|file| file.size).sum();
    let plan = TargetBuildPlan {
        installer: Installer {
            preset: None,
            app: App {
                id: AppId::new("com.example.tool").expect("an id"),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse(version).expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            frontend: Frontend::Console,
            updates: None,
            install: Install {
                scope: InstallScope::User,
                directory: InstallDirectory {
                    user: Some(Template::parse("${location.programs}/tool").expect("a directory")),
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        total_size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
        files: resolved,
        ui_assets: Vec::new(),
    };
    BundleWriter::encode(&plan, &[]).expect("the package encodes")
}

/// Compose a real installer: a template with the package appended.
pub fn compose_installer(template: &Path, output: &Path, package: &[u8]) {
    zup_linux::compose(template, output, package).expect("compose");
}

/// A template that proves the carrier path but runs nothing itself.
///
/// The lifecycle under test never executes the template; it opens the
/// package the template carries. (Running the produced installer as a
/// process is the genuine template's test, below.)
pub fn inert_template() -> &'static str {
    "/bin/true"
}

/// Compose an installer for one fixture version on the inert template.
pub fn compose_fixture(
    scratch: &Path,
    file_name: &str,
    version: &str,
    files: &[FixtureFile],
) -> PathBuf {
    let package = package_bytes(scratch, version, files);
    let output = scratch.join(file_name);
    compose_installer(Path::new(inert_template()), &output, &package);
    output
}

/// A genuine runtime template from the staged toolchain, for the process
/// execution tests.
///
/// Discovered beside the test binary's own target directory rather than
/// spelled out, because the profile (debug or release) is a property of how
/// the tests were built, not of the test. Missing templates name the command
/// that stages them instead of failing on a missing file.
pub fn genuine_template(frontend: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("a test executable");
    // target/<profile>/deps/<test> -> target/<profile>/toolchain/*/<name>
    let profile = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("a profile directory");
    let toolchain = profile.join("toolchain");
    let name = format!("zup-setup-{frontend}-x86_64-unknown-linux-gnu");
    let mut found = Vec::new();
    if let Ok(versions) = std::fs::read_dir(&toolchain) {
        for version in versions.flatten() {
            let candidate = version.path().join(&name);
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    assert!(
        !found.is_empty(),
        "no `{name}` in the staged toolchain under {}.\n\n  \
         The process tests need genuine templates. Build them once:\n    \
         cargo xtask toolchain build",
        toolchain.display()
    );
    found.sort();
    found.pop().expect("a template")
}

/// Run one installer image in-process to a stable outcome.
pub fn run_installer(installer: &Path, state: &Path, action: LinuxAction) -> LinuxOutcome {
    run(&LinuxRunRequest {
        installer: installer.to_path_buf(),
        scope: SelectedScope::User,
        state_root: Some(state.to_path_buf()),
        action,
    })
    .expect("the run reaches a stable outcome")
}

/// Run an installer image as a child process with an isolated environment.
///
/// The child inherits nothing about the parent's profile: every location it
/// can resolve comes from the explicit variables, which is what makes this
/// proof about the installer rather than about the test runner's machine.
pub fn run_installer_process(
    installer: &Path,
    user: &IsolatedUser,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(installer);
    command.args(args);
    command.arg("--state-root").arg(&user.state);
    // A hermetic child: nothing from the parent's environment leaks in, so a
    // passing test cannot be passing because of the developer's own profile.
    command.env_clear();
    for (name, value) in user.child_env() {
        command.env(name, value);
    }
    // A minimal PATH so `sh` resolves for installed scripts.
    command.env("PATH", "/usr/bin:/bin");
    command.output().expect("the installer process runs")
}

/// Run an installed executable and read its output.
pub fn run_tool(tool: &Path, args: &[&str]) -> String {
    let output = Command::new(tool)
        .args(args)
        .output()
        .expect("the installed executable runs");
    assert!(
        output.status.success(),
        "exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 output")
}

pub fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}
