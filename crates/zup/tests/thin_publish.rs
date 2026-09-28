//! `zup publish stage --thin`, end to end against the real binary.
//!
//! This is the claim the whole online story makes, so it is tested the way a
//! user meets it: one command, on a real project, producing the files a person
//! would double-click. What it proves is not that the code runs - the unit tests
//! cover that - but the three promises a thin installer makes.
//!
//! 1. **It is thin.** Four megabytes of content plus a full TUF root fits in a
//!    file barely larger than the launcher it is made of, because the file
//!    carries an index and a trust block and nothing else.
//! 2. **The two files make different promises.** A version-labelled installer
//!    and a channel installer differ in which document they authenticate, and
//!    neither can be made to install the other's release.
//! 3. **Nothing in the launcher is authoritative except the root.** The only
//!    content a thin artifact embeds is the trust block, and it has to be a real
//!    TUF root that parses.
//!
//! One architecture, because the thin runtime is a real PE image and this host
//! builds one. The two-architecture case is a composition concern, and
//! `zup-artifact`'s web suite covers it against the same layout.

#![cfg(windows)]

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;
use zup_artifact::ArtifactMode;
use zup_windows::UniversalArtifact;

#[path = "support/staged.rs"]
mod staged;

/// The machine a thin release is staged for, which is the one this host can build
/// a real runtime template for.
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;

/// A one-architecture project with a TUF root and a repository, which is what a
/// thin release requires.
struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let root_path = root.path();
        let source = root_path.join("src");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::create_dir_all(source.join("assets")).unwrap();
        fs::write(
            source.join("assets/strings.json"),
            payload("the same translations on every machine"),
        )
        .unwrap();
        fs::write(source.join("assets/logo.png"), payload("the same pixels")).unwrap();
        fs::write(source.join("bin/Acme.exe"), b"x86_64 machine code").unwrap();
        // A real TUF root, so the trust block is a document rather than a
        // placeholder. A thin installer that embedded an unparseable root would
        // be useless, and a test that only checked the field was present would
        // not notice.
        fs::write(
            root_path.join("root.json"),
            include_str!("fixtures/root.json"),
        )
        .unwrap();
        fs::write(
            root_path.join("zup.toml"),
            format!(
                r#"
schema = 1
frontend = "gui"
[app]
id = "com.example.thin"
name = "Thin"
version = "1.4.0"
[updates]
repository = "https://updates.example.com/thin"
channel = "stable"
root = "root.json"
[build]

[build.targets.x64]
target = "{HOST_TARGET}"
source = {{ directory = "src" }}

[install]
scope = "user"
[install.directory]
user = "${{location.programs}}/Thin"
[[files]]
source = "**/*"
destination = "${{install}}"
"#
            ),
        )
        .unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn manifest(&self) -> std::path::PathBuf {
        self.path().join("zup.toml")
    }

    /// Stage a thin release with `extra` appended to the argument list.
    ///
    /// The runtime template and the launcher both come from the staged toolchain,
    /// which is what a publisher who installed `zup` has. Only the launcher is
    /// named explicitly, because the test has to measure it.
    fn stage_with(
        &self,
        extra: &[std::ffi::OsString],
    ) -> (std::process::Output, Vec<std::path::PathBuf>) {
        let output = self.path().join("web");
        let mut args: Vec<std::ffi::OsString> = vec![
            "publish".into(),
            "stage".into(),
            "--manifest".into(),
            self.manifest().into(),
            "--output".into(),
            output.clone().into(),
            "--thin".into(),
            "--dispatcher".into(),
            dispatcher().into(),
            "--repository".into(),
            "https://updates.example.com/thin".into(),
        ];
        args.extend(extra.iter().cloned());
        let installers_dir = self.path().join("installers");
        args.push(std::ffi::OsString::from("--thin-output"));
        args.push(installers_dir.clone().into());
        let result = Command::new(env!("CARGO_BIN_EXE_zup"))
            .args(args)
            .output()
            .unwrap();
        let installers = ["Thin-Setup-version.exe", "Thin-Setup-channel.exe"]
            .iter()
            .map(|name| installers_dir.join(name))
            .collect();
        (result, installers)
    }

    fn stage(&self) -> (std::process::Output, Vec<std::path::PathBuf>) {
        self.stage_with(&[])
    }
}

/// Payload large enough that embedding it would be obvious.
fn payload(seed: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut block = 0u64;
    while out.len() < 2 * 1024 * 1024 {
        let digest = <sha2::Sha256 as sha2::Digest>::digest(format!("{seed}{block}").as_bytes());
        out.extend_from_slice(
            zup_core::Sha256Digest::from_bytes(digest.into())
                .to_hex()
                .as_bytes(),
        );
        block += 1;
    }
    out.truncate(2 * 1024 * 1024);
    out
}

fn stderr(result: &std::process::Output) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}

fn stdout(result: &std::process::Output) -> String {
    String::from_utf8_lossy(&result.stdout).into_owned()
}

/// The x86 windowed launcher that can resolve a release over the network, which
/// is what a thin installer is made of. It has to start on the narrowest machine
/// any variant serves.
fn dispatcher() -> std::path::PathBuf {
    staged::dispatcher(&test_executable(), zup_toolchain::Subsystem::Gui, true)
}

/// This test binary, which is where the staged toolchain was built beside.
fn test_executable() -> std::path::PathBuf {
    std::env::current_exe().expect("a test executable")
}

#[test]
fn a_thin_release_stages_a_web_tree_and_two_thin_installers() {
    let project = Project::new();
    let launcher = dispatcher();
    let launcher_size = fs::metadata(&launcher).unwrap().len();
    let (result, installers) = project.stage();
    assert!(result.status.success(), "{}", stderr(&result));

    // The web tree is the release: content, catalog, the variant manifest, the
    // channel pointer, and the version-addressed document.
    let web = project.path().join("web");
    for document in [
        "releases/stable.json",
        "releases/stable/catalog.json",
        "releases/stable/variants/x64.json",
        "releases/stable/versions/1.4.0.json",
    ] {
        assert!(web.join(document).is_file(), "{document} was not staged");
    }
    // The thin runtime is content, because a client verifies it before running
    // it. A bootstrapper that fetched something the graph did not name would
    // have nothing to check it against.
    assert!(
        web.join("blobs").is_dir(),
        "the release carries verified content, including the native runtime"
    );

    // The thin installers are thin. The content is fetched from the release, so a
    // file is the launcher, a trust block, and an index - and nothing else. The
    // claim is relative to the launcher rather than to a byte count, because the
    // launcher's size is a build-profile decision and the ratio between them is
    // the design property.
    for file in &installers {
        assert!(file.is_file(), "{} was not written", file.display());
        let size = fs::metadata(file).unwrap().len();
        assert!(
            size <= launcher_size + 1024 * 1024,
            "a thin installer is a launcher, a trust block, and an index, not an \
             application: {} is {size} bytes against a {launcher_size}-byte launcher",
            file.display()
        );
        let declared = UniversalArtifact::open(file)
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()))
            .index()
            .standalone_size();
        // The content has to be somewhere. If the release were no larger than the
        // launcher, "thin" would be measuring nothing.
        assert!(
            declared > size * 4,
            "the release carries content worth fetching: {declared} bytes of declared \
             content against a {size}-byte installer"
        );
    }

    // The report says what was published and how the two differ, because a
    // publisher who cannot tell which file is which will ship the wrong one.
    let report = stdout(&result);
    assert!(report.contains("Thin-Setup-version.exe"), "{report}");
    assert!(report.contains("Thin-Setup-channel.exe"), "{report}");
    assert!(report.contains("versions/1.4.0.json"), "{report}");
}

#[test]
fn the_two_thin_installers_authenticate_two_different_documents() {
    let project = Project::new();
    let (result, installers) = project.stage();
    assert!(result.status.success(), "{}", stderr(&result));

    let pinned = UniversalArtifact::open(&installers[0])
        .unwrap_or_else(|error| panic!("the version installer: {error}"));
    let channel = UniversalArtifact::open(&installers[1])
        .unwrap_or_else(|error| panic!("the channel installer: {error}"));

    for (artifact, label) in [(&pinned, "version"), (&channel, "channel")] {
        let index = artifact.index();
        assert_eq!(index.artifact.mode, ArtifactMode::Thin, "{label}");
        assert!(
            !index.artifact.mode.carries_content(),
            "{label} carries no content"
        );
        let trust = index
            .artifact
            .trust
            .as_ref()
            .unwrap_or_else(|| panic!("{label} embeds a trust block"));
        trust
            .validate()
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(trust.app_id, index.artifact.application.id);
        assert_eq!(trust.channel, "stable");
        assert_eq!(trust.repository, "https://updates.example.com/thin");
        // The embedded root is the project's root, base64-encoded, and it parses
        // as the TUF document it is. A root that decoded to valid JSON of the
        // wrong shape would still be a useless trust anchor, so the check is for
        // the document a TUF client actually reads.
        let root: serde_json::Value =
            serde_json::from_slice(&trust.root_bytes().expect("the root decodes"))
                .expect("the embedded root is a JSON document");
        assert_eq!(root["signed"]["_type"], "root");
        assert_eq!(root["signed"]["version"], 1);
        assert!(
            root["signed"]["keys"]
                .as_object()
                .is_some_and(|keys| !keys.is_empty()),
            "the root names the keys that vouch for it"
        );
        index
            .artifact
            .validate()
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(index.variant_ids(), vec!["x64"]);
    }

    // The one difference that matters.
    let document = |artifact: &UniversalArtifact| {
        artifact
            .index()
            .artifact
            .trust
            .as_ref()
            .expect("a trust block")
            .release_document()
            .expect("addressable")
            .to_string()
    };
    assert_eq!(document(&pinned), "releases/stable/versions/1.4.0.json");
    assert_eq!(document(&channel), "releases/stable.json");

    // And the two files are genuinely different files, not the same bytes twice.
    assert_ne!(
        fs::read(&installers[0]).unwrap(),
        fs::read(&installers[1]).unwrap(),
        "two files, two promises"
    );
}

/// A thin artifact resolves a release through a trust block, so a manifest with
/// no anchor and a `--channel` that disagrees with the manifest are both refused
/// here rather than on a user's machine, and the refusal names what is wrong.
#[test]
fn a_thin_release_without_a_matching_trust_anchor_is_refused() {
    let mismatched = Project::new();
    let (result, _) =
        mismatched.stage_with(&[std::ffi::OsString::from("--channel"), "beta".into()]);
    assert!(
        !result.status.success(),
        "a mismatched channel must be refused"
    );
    let message = stderr(&result);
    assert!(message.contains("beta"), "{message}");
    assert!(message.contains("stable"), "{message}");

    // A thin artifact with no trust block cannot resolve anything, and
    // discovering that on a user's machine is the worst place to discover it.
    let project = Project::new();
    fs::write(
        project.manifest(),
        format!(
            r#"
schema = 1
frontend = "gui"
[app]
id = "com.example.thin"
name = "Thin"
version = "1.4.0"
[build]

[build.targets.x64]
target = "{HOST_TARGET}"
source = {{ directory = "src" }}

[install]
scope = "user"
[install.directory]
user = "${{location.programs}}/Thin"
[[files]]
source = "**/*"
destination = "${{install}}"
"#
        ),
    )
    .unwrap();

    let (result, _) = project.stage();
    assert!(
        !result.status.success(),
        "a thin release needs a trust anchor"
    );
    assert!(
        stderr(&result).contains("[updates]"),
        "the refusal names what is missing: {}",
        stderr(&result)
    );
}

#[test]
fn a_thin_release_names_the_launcher_it_resolved() {
    // A thin installer *is* a launcher plus a trust block, so which launcher the
    // release was composed with is the difference between a file that installs
    // itself and one that cannot.
    let project = Project::new();
    let output = project.path().join("web");
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .args([
            std::ffi::OsString::from("publish"),
            std::ffi::OsString::from("stage"),
            std::ffi::OsString::from("--manifest"),
            project.manifest().into(),
            std::ffi::OsString::from("--output"),
            output.clone().into(),
            std::ffi::OsString::from("--thin"),
            std::ffi::OsString::from("--dispatcher"),
            console_dispatcher().into(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("launcher"), "{message}");
    assert!(message.contains("gui"), "{message}");
    assert!(
        !output.join("installers").exists(),
        "a refused composition writes no installers"
    );
}

/// The console launcher, which is not the one a GUI frontend's installer is made
/// of.
fn console_dispatcher() -> std::path::PathBuf {
    staged::dispatcher(&test_executable(), zup_toolchain::Subsystem::Console, true)
}
