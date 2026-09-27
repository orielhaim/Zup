//! The build command's artifact surface, end to end.
//!
//! Everything here runs the real `zup` binary against a real project on disk.
//! The portable graph is covered by `zup-artifact` and the Windows resource
//! mechanics by `zup-windows`; what these tests pin down is the part a user
//! touches: the flags, the file that appears, the report that describes it, and
//! the refusal when a composition cannot be honest.

#![cfg(all(feature = "build", windows))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;
use zup_windows::UniversalArtifact;

/// A runtime template the build accepts for one machine.
///
/// The real thing is a compiled `zup-setup` image, one per target, named for the
/// frontend it implements. A test only needs an image the name, machine, and
/// subsystem checks can read, and the runtime is embedded as bytes rather than
/// executed, so a header that names the right machine and subsystem is the whole
/// contract. One directory per target, because the name is part of the contract.
fn runtime_template(root: &Path, profile: &str, machine: u16) -> PathBuf {
    const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;
    let mut bytes = vec![0u8; 0x178];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
    bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
    bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
    bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
    bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
    bytes[0x9c..0x9e].copy_from_slice(&IMAGE_SUBSYSTEM_WINDOWS_CUI.to_le_bytes());
    let directory = root.join(format!("templates/{profile}"));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("zup-setup-console.exe");
    fs::write(&path, &bytes).unwrap();
    path
}

const X64: &str = "x86_64-pc-windows-msvc";
const ARM64: &str = "aarch64-pc-windows-msvc";

/// A two-architecture project whose payload is identical across machines except
/// for one binary each, which is the shape a real application has.
struct Project {
    root: TempDir,
    /// The profiles the manifest declares, in the order the build processes them.
    ///
    /// A per-target flag is positional, and the order is the manifest's own
    /// target order rather than the order they appear in the file, so a test
    /// that assembles `--runtime` has to use this and not declaration order.
    runtimes: Vec<PathBuf>,
}

impl Project {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let root_path = root.path();
        for (profile, target) in [("x64", X64), ("arm64", ARM64)] {
            let source = root_path.join(format!("src-{profile}"));
            fs::create_dir_all(source.join("bin")).unwrap();
            fs::create_dir_all(source.join("assets")).unwrap();
            // Shared bytes: composition must store these once.
            fs::write(
                source.join("assets/strings.json"),
                shared_bytes("the same translations on every machine"),
            )
            .unwrap();
            fs::write(
                source.join("assets/logo.png"),
                shared_bytes("the same pixels on every machine"),
            )
            .unwrap();
            // Architecture-specific bytes: composition must keep these apart.
            fs::write(
                source.join("bin/Acme.exe"),
                format!("{target} machine code").into_bytes(),
            )
            .unwrap();
        }
        fs::write(
            root_path.join("zup.toml"),
            r#"
schema = 1
frontend = "console"
[app]
id = "com.example.universal"
name = "Universal"
version = "1.4.0"
[build]

[build.targets.arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "src-arm64" }

[build.targets.x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "src-x64" }

[install]
scope = "user"
[install.directory]
user = "${location.programs}/Universal"
[[files]]
source = "**/*"
destination = "${install}"
"#,
        )
        .unwrap();
        let runtimes = vec![
            runtime_template(root_path, "arm64", 0xaa64),
            runtime_template(root_path, "x64", 0x8664),
        ];
        Self { root, runtimes }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn manifest(&self) -> PathBuf {
        self.path().join("zup.toml")
    }

    /// `--runtime` for every selected target, in the order the build wants them.
    fn runtime_args(&self) -> Vec<std::ffi::OsString> {
        self.runtimes
            .iter()
            .flat_map(|path| {
                [
                    std::ffi::OsString::from("--runtime"),
                    path.as_os_str().to_owned(),
                ]
            })
            .collect()
    }

    /// Run `zup build --universal` over every target.
    fn build_universal(&self, output: &Path, dispatcher: &Path) -> Output {
        self.build_universal_with(output, dispatcher, "none")
    }

    /// Run `zup build --universal` over every target, choosing where the
    /// machine-readable release description goes.
    fn build_universal_with(&self, output: &Path, dispatcher: &Path, release: &str) -> Output {
        let args: Vec<std::ffi::OsString> = [
            vec![
                std::ffi::OsString::from("build"),
                std::ffi::OsString::from("--manifest"),
                self.manifest().into_os_string(),
                std::ffi::OsString::from("--universal"),
            ],
            self.runtime_args(),
            vec![
                std::ffi::OsString::from("--dispatcher"),
                dispatcher.as_os_str().to_owned(),
                std::ffi::OsString::from("--output"),
                output.as_os_str().to_owned(),
                std::ffi::OsString::from("--force"),
                std::ffi::OsString::from("--release-manifest"),
                std::ffi::OsString::from(release),
            ],
        ]
        .concat();
        zup_owned(&args)
    }
}

/// Compressible, non-repeating bytes, so a shared file really is shared rather
/// than coincidentally equal.
///
/// The size matters: a megabyte-scale payload is what an application has, and it
/// is the only scale at which storing shared content once beats shipping two
/// copies. A fixture measured in kilobytes would be dominated by the dispatcher
/// and would prove nothing.
fn shared_bytes(seed: &str) -> Vec<u8> {
    const SIZE: usize = 4 * 1024 * 1024;
    let mut out = Vec::new();
    let mut block = 0u64;
    while out.len() < SIZE {
        out.extend_from_slice(
            zup_core::Sha256Digest::from_bytes(
                <sha2::Sha256 as sha2::Digest>::digest(format!("{seed}{block}").as_bytes()).into(),
            )
            .to_hex()
            .as_bytes(),
        );
        block += 1;
    }
    out.truncate(SIZE);
    out
}

fn zup(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(args)
        .output()
        .unwrap()
}

fn zup_owned(args: &[std::ffi::OsString]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(args)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A dispatcher image by name, from the build step that installs it.
fn dispatcher(name: &str) -> PathBuf {
    zup_xtask::dispatcher::beside_test_executable(
        &std::env::current_exe().expect("test executable"),
        name,
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

/// The console dispatcher, which is what a console artifact is composed into.
fn console_dispatcher() -> PathBuf {
    dispatcher(zup_xtask::dispatcher::CONSOLE)
}

/// `--universal` on two architectures: one file, two variants, shared content
/// stored once.
#[test]
fn a_universal_build_writes_one_file_containing_every_selected_target() {
    let project = Project::new();
    let output = project.path().join("Universal-Windows-Setup.exe");
    let result = project.build_universal(&output, &console_dispatcher());
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(output.is_file(), "one artifact is written for both targets");

    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    let index = artifact.index();
    assert_eq!(index.artifact.kind, zup_artifact::ArtifactKind::Universal);
    assert_eq!(index.artifact.mode, zup_artifact::ArtifactMode::Offline);
    assert_eq!(index.artifact.pin.label(), "1.4.0");
    assert_eq!(index.variant_ids(), vec!["arm64", "x64"]);
    for id in index.variant_ids() {
        artifact
            .view()
            .verify_variant(id)
            .unwrap_or_else(|error| panic!("{id} is complete: {error}"));
    }

    let report = stdout(&result);
    assert!(report.contains("Kind        Universal"), "{report}");
    assert!(report.contains("Mode        Offline"), "{report}");
    assert!(
        report.contains("aarch64-pc-windows-msvc console, x86_64-pc-windows-msvc console"),
        "both variants are named: {report}"
    );

    // The reported saving is real: the artifact is smaller than two installers.
    let mut references: std::collections::BTreeMap<zup_core::Sha256Digest, usize> =
        std::collections::BTreeMap::new();
    for id in index.variant_ids() {
        for digest in artifact
            .view()
            .variant_manifest(id)
            .unwrap()
            .content_digests()
        {
            *references.entry(digest).or_default() += 1;
        }
    }
    assert!(
        references.values().any(|count| *count > 1),
        "the shared assets are referenced by both variants"
    );
    let file = fs::metadata(&output).unwrap().len();
    let two_installers = index
        .variants
        .iter()
        .map(|variant| variant.logical_size)
        .sum::<u64>()
        + project
            .runtimes
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .sum::<u64>();
    // The comparison is between the two shapes' *content*, so the launcher's one
    // time cost is subtracted from the composed side. A universal artifact needs
    // one dispatcher and two standalone installers need none, so including it
    // would measure the launcher rather than the composition — and the launcher is
    // the same file in both cases once the second variant exists.
    let launcher = fs::metadata(console_dispatcher()).unwrap().len();
    let content = file.saturating_sub(launcher);
    assert!(
        content < two_installers,
        "composing two variants stores less content than shipping two installers: \
         {content} bytes of content and a {launcher}-byte launcher, \
         against {two_installers} bytes of installers"
    );
    // And the sharing is the reason: the same two assets on two machines cost
    // roughly half of two whole copies, not the whole of both.
    assert!(
        content * 2 < two_installers * 3 / 2,
        "shared content is stored once: {content} against {two_installers}"
    );
}

/// `zup artifact inspect` reads the same file and says the same things, in
/// text and in the versioned report.
#[test]
fn inspect_describes_the_built_artifact_in_text_and_json() {
    let project = Project::new();
    let output = project.path().join("Universal-Windows-Setup.exe");
    let built = project.build_universal(&output, &console_dispatcher());
    assert!(built.status.success(), "{}", stderr(&built));

    let human = zup(&["artifact".as_ref(), "inspect".as_ref(), output.as_os_str()]);
    assert!(human.status.success(), "{}", stderr(&human));
    let text = stdout(&human);
    assert!(
        text.contains("Windows universal offline installer"),
        "{text}"
    );
    assert!(text.contains("x86_64-pc-windows-msvc"), "{text}");
    assert!(text.contains("aarch64-pc-windows-msvc"), "{text}");
    assert!(text.contains("Authenticode             unsigned"), "{text}");
    assert!(text.contains("content digests          valid"), "{text}");

    let json = zup(&[
        "artifact".as_ref(),
        "inspect".as_ref(),
        output.as_os_str(),
        "--format".as_ref(),
        "json".as_ref(),
    ]);
    assert!(json.status.success(), "{}", stderr(&json));
    let report: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(report["report_version"], 1);
    assert_eq!(report["kind"], "universal");
    assert_eq!(report["mode"], "offline");
    assert_eq!(report["pin"], "1.4.0");
    assert_eq!(report["subsystem"], "console");
    assert_eq!(report["application"], "Universal");
    assert_eq!(report["application_version"], "1.4.0");
    assert_eq!(report["trust"]["authenticode"], "unsigned");
    assert_eq!(report["trust"]["content_digests"], "valid");
    assert_eq!(report["variants"].as_array().unwrap().len(), 2);
    // Every blob is either needed by more than one variant or by exactly one, so
    // the two sizes partition the uncompressed content. The stored size is what
    // compression made of it, and the logical size counts the shared bytes once
    // per variant that wanted them.
    let content = &report["content"];
    let shared = content["shared_size"].as_u64().unwrap();
    let exclusive = content["exclusive_size"].as_u64().unwrap();
    let stored = content["stored_size"].as_u64().unwrap();
    let uncompressed = content["content_size"].as_u64().unwrap();
    assert!(shared > 0, "the shared assets are stored once");
    assert_eq!(
        shared + exclusive,
        uncompressed,
        "every blob is either shared or exclusive"
    );
    assert!(stored < uncompressed, "the store compresses");
    assert!(
        content["logical_size"].as_u64().unwrap() > uncompressed,
        "the logical size counts shared bytes again for the second variant"
    );
    assert!(content["unique_blob_count"].as_u64().unwrap() > 0);
}

/// The JSON report is the contract; nothing in it is a build-machine path.
#[test]
fn a_release_description_names_files_rather_than_build_paths() {
    let project = Project::new();
    let output = project.path().join("Universal-Windows-Setup.exe");
    let built =
        project.build_universal_with(&output, &console_dispatcher(), "dist/zup-release.json");
    assert!(built.status.success(), "{}", stderr(&built));
    let description = project.path().join("dist/zup-release.json");
    assert!(description.is_file(), "{}", stdout(&built));

    let text = fs::read_to_string(&description).unwrap();
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["root"], ".");
    assert_eq!(report["application"]["id"], "com.example.universal");
    assert_eq!(report["application"]["version"], "1.4.0");
    let variants = report["variants"].as_array().expect("variants");
    assert_eq!(variants.len(), 2, "each variant is described once");
    for variant in variants {
        assert_eq!(variant["platform"]["os"], "windows");
        assert!(variant["id"].is_string());
        assert!(variant["target"].is_string());
    }
    let artifacts = report["artifacts"].as_array().expect("artifacts");
    assert_eq!(artifacts.len(), 1);
    let entry = &artifacts[0];
    assert_eq!(entry["path"], "Universal-Windows-Setup.exe");
    assert_eq!(entry["kind"], "universal");
    assert_eq!(entry["mode"], "offline");
    assert_eq!(entry["subsystem"], "console");
    assert_eq!(entry["size"], fs::metadata(&output).unwrap().len());
    assert_eq!(
        entry["digest"].as_str().unwrap().len(),
        64,
        "a content digest, so a publisher can verify what it shipped"
    );
    assert_eq!(entry["variants"].as_array().unwrap().len(), 2);
    // No build-machine path survives into a published description.
    assert!(
        !text.contains(&project.path().display().to_string()),
        "the description names files, not the directory they were built in"
    );
    let path = entry["path"].as_str().unwrap();
    assert!(!path.contains('/') && !path.contains('\\'), "{path}");
}

/// `--target` and `--artifact` are different questions, and a project that
/// declares nothing still gets today's behaviour: one ordinary installer per
/// selected target, composed by writing resources into a real runtime image.
#[test]
#[cfg(feature = "console")]
fn a_project_with_no_declared_artifacts_still_builds_one_installer_per_target() {
    let project = Project::new();
    let output = project.path().join("Setup.exe");
    let result = zup(&[
        "build".as_ref(),
        "--manifest".as_ref(),
        project.manifest().as_os_str(),
        "--target".as_ref(),
        "x64".as_ref(),
        "--runtime".as_ref(),
        Path::new(env!("CARGO_BIN_EXE_zup-setup-console")).as_os_str(),
        "--output".as_ref(),
        output.as_os_str(),
        "--force".as_ref(),
        "--release-manifest".as_ref(),
        "none".as_ref(),
    ]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(output.is_file());
    // A single-target build is an ordinary package, not a composed artifact.
    let bundle = zup_windows::EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.target().as_str(), X64);
    assert_eq!(bundle.frontend(), zup_core::Frontend::Console);
    assert_eq!(
        zup_windows::read_pe_subsystem(&output).unwrap(),
        zup_windows::PeSubsystem::Console
    );
    // Nothing a composed artifact would have written is there.
    assert!(
        zup_windows::UniversalArtifact::open(&output).is_err(),
        "a single-target installer is not a universal artifact"
    );
}

/// A per-target flag that does not line up with the selected targets names them,
/// because the order is the manifest's own and not the order they were written.
#[test]
fn a_wrong_number_of_runtime_templates_names_the_target_order() {
    let project = Project::new();
    let result = zup(&[
        "build".as_ref(),
        "--manifest".as_ref(),
        project.manifest().as_os_str(),
        "--universal".as_ref(),
        "--runtime".as_ref(),
        project.runtimes[0].as_os_str(),
        "--output".as_ref(),
        project.path().join("Setup.exe").as_os_str(),
        "--force".as_ref(),
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(
        message.contains("selected 2 targets (arm64, x64)"),
        "{message}"
    );
    assert!(message.contains("received 1 runtimes"), "{message}");
    assert!(message.contains("in that order"), "{message}");
}

/// Naming an artifact that does not exist is a refusal that lists the ones that
/// do, not a silent fallback to a default.
#[test]
fn an_unknown_artifact_name_lists_the_declared_ones() {
    let project = Project::new();
    let result = zup(&[
        "build".as_ref(),
        "--manifest".as_ref(),
        project.manifest().as_os_str(),
        "--artifact".as_ref(),
        "installer".as_ref(),
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(
        message.contains("unknown artifact `installer`"),
        "{message}"
    );
    assert!(message.contains("declared artifacts are none"), "{message}");
}

/// A dispatcher built for the wrong launcher experience is refused before
/// anything is written.
#[test]
fn a_dispatcher_for_the_wrong_launcher_experience_is_refused() {
    let project = Project::new();
    let output = project.path().join("Universal-Windows-Setup.exe");
    let result = project.build_universal(&output, &dispatcher(zup_xtask::dispatcher::GUI));
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("launcher"), "{message}");
    assert!(!output.exists(), "a refused build writes nothing");
}

/// A runtime template for the wrong machine is refused with both targets named,
/// because it is a per-target mistake and the message has to say which.
#[test]
fn a_runtime_template_for_the_wrong_machine_is_refused() {
    let project = Project::new();
    let result = zup(&[
        "build".as_ref(),
        "--manifest".as_ref(),
        project.manifest().as_os_str(),
        "--target".as_ref(),
        "x64".as_ref(),
        "--runtime".as_ref(),
        project.runtimes[0].as_os_str(),
        "--output".as_ref(),
        project.path().join("Setup.exe").as_os_str(),
        "--force".as_ref(),
        "--release-manifest".as_ref(),
        "none".as_ref(),
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains(ARM64), "{message}");
    assert!(message.contains(X64), "{message}");
    assert!(
        message.contains("does not match"),
        "the refusal names both machines: {message}"
    );
}

/// A template for the right machine but the wrong frontend is refused, and a
/// universal build refuses it exactly the way a single-target build does.
#[test]
fn a_runtime_template_for_the_wrong_frontend_is_refused_by_both_paths() {
    let project = Project::new();
    let headless = project.path().join("templates/headless");
    fs::create_dir_all(&headless).unwrap();
    let bytes = fs::read(&project.runtimes[1]).unwrap();
    let template = headless.join("zup-setup-headless.exe");
    fs::write(&template, &bytes).unwrap();

    let single = project.path().join("Single.exe");
    let result = zup(&[
        "build".as_ref(),
        "--manifest".as_ref(),
        project.manifest().as_os_str(),
        "--target".as_ref(),
        "x64".as_ref(),
        "--runtime".as_ref(),
        template.as_os_str(),
        "--output".as_ref(),
        single.as_os_str(),
        "--force".as_ref(),
        "--release-manifest".as_ref(),
        "none".as_ref(),
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("not the console template"), "{message}");

    let universal = project.path().join("Universal-Windows-Setup.exe");
    let args: Vec<std::ffi::OsString> = [
        vec![
            std::ffi::OsString::from("build"),
            std::ffi::OsString::from("--manifest"),
            project.manifest().into_os_string(),
            std::ffi::OsString::from("--universal"),
        ],
        vec![
            std::ffi::OsString::from("--runtime"),
            template.as_os_str().to_owned(),
            std::ffi::OsString::from("--runtime"),
            project.runtimes[0].as_os_str().to_owned(),
        ],
        vec![
            std::ffi::OsString::from("--dispatcher"),
            console_dispatcher().into_os_string(),
            std::ffi::OsString::from("--output"),
            universal.as_os_str().to_owned(),
            std::ffi::OsString::from("--force"),
            std::ffi::OsString::from("--release-manifest"),
            std::ffi::OsString::from("none"),
        ],
    ]
    .concat();
    let result = zup_owned(&args);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(
        message.contains("not the console template"),
        "a universal artifact refuses the template a single-target build refuses: {message}"
    );
}
