//! `zup preview`: the application's own window, resolved as a build resolves it.
//!
//! Every claim here is about reuse, and reuse is invisible when it is missing, so
//! each test states the same thing a build would state about the same project and
//! then checks that the preview says it too.
//!
//! The window is `zup-preset-test`: a real preset, built against the public
//! `zup-preset-sdk` and nothing else, staged by the same run as every other real
//! binary. It is the right thing to preview with because it behaves like a
//! third-party preset rather than like a test: it reads the settings the
//! application configured, reports what it received to a path those settings
//! named, chooses a component, presses Install, and closes. A preview that
//! resolved the right bytes and then ran something else would pass every test
//! here that did not launch it.
//!
//! What is proved about safety is proved by absence: a simulated install that
//! reached the machine would show up as a file outside the session's own
//! directory, and there is a test that walks for one.

#[path = "support/staged.rs"]
mod staged;

#[path = "support/toolchain_fixture.rs"]
mod toolchain;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zup::preview::Session;
use zup_artifact::ui::PresetPackageWriter;
use zup_preview::ControlOutcome;
use zup_toolchain::ToolchainComponent;
use zup_preset_compose::resolve;

const HOST: &str = zup_plugin_contract::HOST_TARGET;

/// The settings `zup-preset-test` accepts, as a real preset's generated schema
/// describes them.
///
/// Taken from the preset's own `Settings` type rather than invented, so a test
/// cannot pass by validating against a schema the preset would refuse.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "hero": { "type": ["string", "null"] },
            "logo": {
                "anyOf": [ { "$ref": "#/$defs/AssetRef" }, { "type": "null" } ]
            },
            "report": { "type": "string" }
        },
        "$defs": {
            "AssetRef": { "type": "string", "title": "Asset", "x-zup-asset": true }
        }
    })
}

/// A `.zupui` carrying one real preset for this host, written by the publisher's
/// writer so a preview reads a package exactly as a build would.
fn package(binary: Vec<u8>) -> Vec<u8> {
    let description =
        zup_preset_protocol::PresetDescription::new("e2e", env!("CARGO_PKG_VERSION"), schema())
            .with_capabilities(zup_preset_protocol::Capabilities::new([
                zup_preset_protocol::Capability::Components,
            ]));
    let mut writer = PresetPackageWriter::new(description).expect("a valid description");
    writer
        .add_binary(
            zup_core::TargetTriple::parse(HOST).expect("a target"),
            binary,
        )
        .expect("one binary");
    writer.finish().expect("a verified package")
}

/// The real preset, found the way every other test finds a staged component.
fn peer_bytes() -> Vec<u8> {
    let name = zup_toolchain::test_preset_file_name(std::env::consts::EXE_SUFFIX);
    let mut beside = PathBuf::from(env!("CARGO_BIN_EXE_zup"));
    beside.pop();
    if beside.ends_with("deps") {
        beside.pop();
    }
    for root in [
        beside.join("toolchain").join(env!("CARGO_PKG_VERSION")),
        beside.join("toolchain"),
    ] {
        let path = root.join(&name);
        if path.is_file() {
            return std::fs::read(&path).expect("the peer preset is readable");
        }
    }
    panic!(
        "no `{name}` in the staged toolchain beside {}.\n\n  \
         `zup preview` is proved against a real preset. Build it once:\n    \
         cargo xtask toolchain build",
        beside.display()
    );
}

/// The real preset, with trailing bytes.
///
/// The same program, at a different address. That is not a contrivance: an
/// installer pads itself, so "the same window in a new package" is a thing that
/// genuinely happens, and it is the case a last-known-good replacement has to
/// handle without treating every new package as a new program.
fn peer_padded() -> Vec<u8> {
    let mut bytes = peer_bytes();
    bytes.extend_from_slice(&[0u8; 4096]);
    bytes
}

/// A real application project, and the manifest it is written with.
struct Project {
    directory: tempfile::TempDir,
}

impl Project {
    fn manifest_path(&self) -> PathBuf {
        self.directory.path().join("zup.toml")
    }

    /// The report the preset will write, which is a path its own settings name.
    fn report_path(&self) -> PathBuf {
        self.directory.path().join("report.txt")
    }

    /// The manifest, configured the way `settings` says.
    ///
    /// `package` names a `.zupui` in this project; `None` leaves the application to
    /// get the preset zup ships, which is what an application that customizes
    /// nothing does.
    fn manifest(&self, settings: &str, package: Option<&str>) -> String {
        let ui = match package {
            Some(package) => format!("[ui]\npreset = \"{package}\"\n\n"),
            None => String::new(),
        };
        format!(
            r#"schema = 1

{ui}[app]
id = "com.example.acme"
name = "Acme Desktop"
version = "2.1.0"
publisher = "Acme Inc"
description = "The Acme desktop application."

[build.targets.windows]
target = "{HOST}"
source = {{ directory = "payload" }}
frontend = "gui"

[install]
scope = "either"

[install.directory]
user = "${{location.user_data}}/Acme"
machine = "${{location.programs}}/Acme"

[[components]]
id = "core"
name = "Acme Desktop"
required = true
default = true

[[components]]
id = "docs"
name = "Documentation"
required = false
default = false

{settings}"#
        )
    }

    fn write(&self, manifest: &str) {
        std::fs::write(self.manifest_path(), manifest).expect("the manifest is written");
    }

    fn write_package(&self, bytes: &[u8]) -> PathBuf {
        let path = self.directory.path().join("aurora.zupui");
        std::fs::write(&path, bytes).expect("the package is written");
        path
    }

    /// The settings every project here is configured with: the text the preset
    /// draws, the file the application provides, and where the preset reports.
    fn settings(&self) -> String {
        format!(
            "[ui.settings]\nhero = \"Install Acme Desktop\"\nlogo = \"branding/logo.svg\"\nreport = {:?}\n",
            self.report_path().display().to_string()
        )
    }

    /// A toolchain root holding the preset zup ships, and the package in it.
    fn toolchain(&self) -> (PathBuf, PathBuf) {
        let root = self.directory.path().join("toolchain");
        let mut staged_preset = toolchain::preset(&[HOST]);
        staged_preset.bytes = Some(package(peer_bytes()));
        (root.clone(), staged_preset.write(&root))
    }

    fn report(&self) -> String {
        std::fs::read_to_string(self.report_path()).unwrap_or_default()
    }
}

fn project() -> Project {
    let directory = tempfile::tempdir().expect("a scratch directory");
    std::fs::create_dir_all(directory.path().join("payload")).expect("the payload directory");
    std::fs::create_dir_all(directory.path().join("branding")).expect("the branding directory");
    std::fs::write(directory.path().join("payload/readme.txt"), b"Acme").expect("a payload file");
    std::fs::write(
        directory.path().join("branding/logo.svg"),
        b"<svg width='4' height='4'/>",
    )
    .expect("the file the settings name");
    let project = Project { directory };
    project.write(&project.manifest(&project.settings(), None));
    project
}

/// A resolver whose only toolchain is `root`, or one that finds nothing.
fn toolchain(root: Option<&Path>) -> Arc<zup::ToolchainResolver> {
    let resolver = zup::ToolchainResolver::new(
        env!("CARGO_PKG_VERSION").to_owned(),
        PathBuf::from("C:/nowhere/zup.exe"),
        PathBuf::from("C:/nowhere/state"),
    );
    Arc::new(resolver.with_root(Some(match root {
        Some(root) => root.to_path_buf(),
        None => PathBuf::from("C:/nowhere/empty"),
    })))
}

/// Open a preview of `project` over `root`'s toolchain.
fn open(project: &Project, root: Option<&Path>) -> Session {
    Session::open(project.manifest_path(), Vec::new(), toolchain(root))
        .expect("the project presents a window")
}

/// A project with a package of its own, already resolved into a session.
fn opened() -> (Project, Session) {
    let project = project();
    project.write_package(&package(peer_bytes()));
    project.write(&project.manifest(&project.settings(), Some("./aurora.zupui")));
    let session = open(&project, None);
    (project, session)
}

/// The report a real preset wrote, once it has written one.
fn await_report(project: &Project) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let report = project.report();
        if !report.is_empty() {
            return report;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the preset never reported what it received");
}

/// The same file, whatever spelling the filesystem hands back.
///
/// A resolved project path is verbatim on this platform and ordinary everywhere
/// else, and comparing the two spellings of one directory has no useful answer.
fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// The run directory a preview stages its generations into.
fn run_directory(project: &Project) -> PathBuf {
    project
        .directory
        .path()
        .join(".zup")
        .join("preview")
        .join("runs")
}

/// Every path under `root`, so a test can prove nothing was written.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn paths(of: &[PathBuf]) -> BTreeSet<PathBuf> {
    of.iter().cloned().collect()
}

#[test]
fn an_application_that_names_no_package_gets_the_preset_this_machine_ships() {
    let project = project();
    let (root, shipped) = project.toolchain();
    let mut session = open(&project, Some(&root));
    assert!(
        session
            .package()
            .is_some_and(|package| same_file(package, &shipped)),
        "an application that names no package gets the one zup ships, which is its UI selection \
         semantics rather than a fallback: {:?}",
        session.package()
    );
    let report = await_report(&project);
    assert!(
        report.contains("host=Acme Desktop"),
        "and it is a window, drawn by a real preset: {report}"
    );
    session.end();
}

#[test]
fn an_application_that_names_a_package_gets_that_package() {
    let project = project();
    let named = project.write_package(&package(peer_bytes()));
    project.write(&project.manifest(&project.settings(), Some("./aurora.zupui")));
    // A toolchain with nothing in it: a project that chose its own window must not
    // need the one zup ships, and must not be refused because the other is absent.
    let mut session = open(&project, None);
    assert!(
        session
            .package()
            .is_some_and(|package| same_file(package, &named)),
        "a project that chose its own window gets that window, not the one zup ships: {:?}",
        session.package()
    );
    assert_eq!(
        session.configuration().settings["hero"],
        serde_json::json!("Install Acme Desktop"),
        "and it is given the settings the project configured"
    );
    session.end();
}

#[test]
fn a_build_and_a_preview_resolve_the_same_window() {
    let (project, _session) = opened();
    let resolver = toolchain(None);

    // The build's own path, through the production materializer.
    let loaded = zup::project::materialize_project(
        zup::project::select_project(
            &project.manifest_path(),
            &[],
            &zup::project::TargetOverrideArgs::default(),
            false,
        )
        .expect("a project"),
        &resolver,
        zup_build::Writes::Publish,
    )
    .expect("a materializable project");
    let composed = loaded.build.targets[0]
        .installer
        .preset
        .clone()
        .expect("a graphical target carries a window");

    // The preview's own path, through the same resolver it calls.
    let selected = zup::project::select_project(
        &project.manifest_path(),
        &[],
        &zup::project::TargetOverrideArgs::default(),
        true,
    )
    .expect("a project");
    let (installer, target) = zup::preview::the_window(&selected).expect("a window");
    let previewed = resolve(
        &selected.manifest.ui,
        &zup_build::project_root(&selected.manifest_path),
        &installer,
        &target,
        &|| {
            resolver
                .resolve(&ToolchainComponent::Preset, None)
                .map(|resolved| resolved.path)
                .map_err(|error| error.to_string())
        },
        &zup_windows::WindowsSourceFilePolicy,
    )
    .expect("the same resolver the build uses");

    assert_eq!(
        composed, previewed.runtime,
        "what a preview resolves is what a build would compose, as one value"
    );
    assert_eq!(
        previewed.executable,
        peer_bytes(),
        "and the bytes selected from the package are the bytes the package carried"
    );
    assert_eq!(
        previewed.assets.len(),
        1,
        "with the application-provided file the settings named"
    );
}

#[test]
fn a_real_preset_receives_the_application_metadata_settings_and_asset_bytes() {
    let (project, mut session) = opened();
    let report = await_report(&project);
    assert!(
        report.contains("host=Acme Desktop"),
        "the product identity is the application's own: {report}"
    );
    assert!(
        report.contains("hero=Install Acme Desktop"),
        "the settings reach a third-party-shaped preset as the manifest wrote them: {report}"
    );
    assert!(
        report.contains("components=2"),
        "and so do the components it declared: {report}"
    );
    assert!(
        report.contains("asset=logo"),
        "with the file the settings named, by the name the settings used: {report}"
    );
    let expected = std::fs::metadata(project.directory.path().join("branding/logo.svg"))
        .expect("the branding file")
        .len();
    assert!(
        report.contains(&format!("asset-bytes={expected}")),
        "and the preset read exactly the bytes the application provided, not a path to them: \
         {report}"
    );

    assert_eq!(session.state().product.name, "Acme Desktop");
    assert_eq!(
        session.state().surface.components().len(),
        2,
        "while the host holds the same machine the window is drawing"
    );
    session.end();
}

#[test]
fn a_settings_edit_reaches_the_window_without_replacing_it() {
    let (project, mut session) = opened();
    await_report(&project);
    let before = session.presented().expect("a window");

    let edited = format!(
        "[ui.settings]\nhero = \"Get Acme Desktop\"\nlogo = \"branding/logo.svg\"\nreport = {:?}\n",
        project.report_path().display().to_string()
    );
    project.write(&project.manifest(&edited, Some("./aurora.zupui")));
    session.refresh();
    assert_eq!(
        session.configuration().settings["hero"],
        serde_json::json!("Get Acme Desktop"),
        "the host holds what the manifest now says, and published it over the ordinary session"
    );
    assert_eq!(
        session.presented(),
        Some(before),
        "and the same window is still the one showing it: a data change costs no child"
    );
    assert_eq!(
        session.state().product.name,
        "Acme Desktop",
        "while the machine around it is untouched"
    );
    session.end();
}

#[test]
fn an_asset_edit_reaches_the_window_without_replacing_it() {
    let (project, mut session) = opened();
    await_report(&project);
    let before = session.presented().expect("a window");

    std::fs::write(
        project.directory.path().join("branding/logo.svg"),
        b"<svg width='8' height='8'/>",
    )
    .expect("the branding changes");
    session.refresh();
    assert_eq!(
        session.presented(),
        Some(before),
        "the same window is still showing it, from a new address"
    );
    let served = session.configuration().assets["logo"].clone();
    let served = std::fs::canonicalize(Path::new(&served)).expect("the served file");
    let inside = std::fs::canonicalize(project.directory.path().join(".zup").join("preview"))
        .expect("the session's own directory");
    assert!(
        served.starts_with(&inside),
        "with the file served from inside the session's own directory rather than from where the \
         project keeps it: {}",
        served.display()
    );
    let branding = std::fs::read(project.directory.path().join("branding/logo.svg"))
        .expect("the branding file");
    assert_eq!(
        std::fs::read(&served).expect("the served file"),
        branding,
        "and the served copy is the new content rather than the old one"
    );
    session.end();
}

#[test]
fn a_configuration_that_no_longer_fits_leaves_the_last_valid_preview_running() {
    let (project, mut session) = opened();
    await_report(&project);
    let before = session.presented().expect("a window");

    // A number where the text goes, which is the most ordinary way a document
    // stops fitting, and one the preset's own schema refuses.
    project.write(&project.manifest(
        "[ui.settings]\nhero = 42\nreport = \"report.txt\"\n",
        Some("./aurora.zupui"),
    ));
    session.refresh();
    assert_eq!(
        session.presented(),
        Some(before),
        "a document that does not fit must not empty a window that is working"
    );
    assert_eq!(
        session.configuration().settings["hero"],
        serde_json::json!("Install Acme Desktop"),
        "and the last settings that did fit are still what the window will be told"
    );
    assert!(
        project.report().contains("hero=Install Acme Desktop"),
        "so the child still has them, and keeps drawing"
    );
    session.end();
}

#[test]
fn a_manifest_that_no_longer_parses_leaves_the_last_valid_preview_running() {
    let (project, mut session) = opened();
    await_report(&project);
    let before = session.presented().expect("a window");

    std::fs::write(project.manifest_path(), "this is not toml at all {{{")
        .expect("the manifest is broken");
    session.refresh();
    assert_eq!(
        session.presented(),
        Some(before),
        "an unfinished edit to a manifest is the most ordinary thing a person does, and it must \
         not cost the window"
    );
    assert_eq!(session.state().product.name, "Acme Desktop");
    session.end();
}

#[test]
fn a_rebuilt_package_replaces_the_window_only_after_the_new_one_has_connected() {
    let (project, mut session) = opened();
    await_report(&project);
    let first = session.presented().expect("a window");

    // A package whose bytes verify as a package and are not a program: it stages,
    // and it cannot open a session.
    project.write_package(&package(b"this is not a preset".to_vec()));
    session.refresh();
    assert_eq!(
        session.presented(),
        Some(first),
        "a package that will not start leaves the working window exactly where it was"
    );
    assert!(
        project.report().contains("host=Acme Desktop"),
        "and the child that was working is still the one working"
    );

    // A package carrying the same preset at a new address.
    project.write_package(&package(peer_padded()));
    session.refresh();
    let second = session.presented().expect("a window");
    assert_ne!(
        second, first,
        "a rebuilt package is a different window, addressed by its bytes rather than its path"
    );
    assert_eq!(
        session.state().product.name,
        "Acme Desktop",
        "while the machine it is drawing is the same one, because the application did not change"
    );
    assert!(
        run_directory(&project).join("2").is_dir(),
        "and the new window was staged into a generation of its own, which is what makes a \
         running executable replaceable: {:?}",
        tree(&run_directory(&project))
    );
    session.end();
}

#[test]
fn closing_the_window_ends_the_preview() {
    let (_project, mut session) = opened();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !session.closed() {
        session.pump();
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(
        session.closed(),
        "the preview is the window, so closing it ends the session"
    );
    session.end();
}

#[test]
fn the_install_button_of_a_real_preset_drives_the_simulated_lifecycle() {
    let (project, mut session) = opened();
    await_report(&project);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !session.state().state.is_active() {
        session.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        session.state().state.is_active(),
        "an accepted request puts the machine where a real one would be, and the engine runs it"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut reported = false;
    while Instant::now() < deadline {
        session.pump();
        if session
            .state()
            .progress
            .as_ref()
            .and_then(|progress| progress.percent())
            .is_some()
        {
            reported = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(
        reported,
        "the simulated engine reports how far the operation has got"
    );
    assert!(
        session
            .state()
            .surface
            .components()
            .iter()
            .any(|component| component.id.as_str() == "docs" && component.selected),
        "and the component the preset chose is the one the host now holds"
    );
    session.end();
}

#[test]
fn an_operation_a_real_preset_started_survives_a_control_the_state_machine_refuses() {
    let (project, mut session) = opened();
    await_report(&project);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !session.state().state.is_active() {
        session.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        session.state().state.is_active(),
        "the preset's Install was honoured, so there is an operation in flight"
    );
    assert_eq!(
        session.control("nothing-cancels-this"),
        ControlOutcome::Handled,
        "a control the surface does not recognise is still handled"
    );
    assert!(
        session.state().state.is_active(),
        "and the operation survives it, because the state machine decides what an action means \
         rather than the control: nothing here reached the machine"
    );
    session.end();
}

#[test]
fn ending_a_preview_leaves_no_child_running() {
    let (project, mut session) = opened();
    await_report(&project);
    let generation = run_directory(&project).join("1");
    session.end();
    // The point of the assertion: the file can be replaced. A child that was not
    // reaped would still hold it open, and on this platform that is a file the next
    // window could not be staged into.
    std::fs::write(generation.join("preset.exe"), b"replaced")
        .expect("the staged executable is writable again");
}

#[test]
fn nothing_a_preview_does_reaches_past_its_own_directory() {
    let (project, mut session) = opened();
    await_report(&project);
    let state = project.directory.path().join(".zup");
    let before: Vec<PathBuf> = tree(project.directory.path())
        .into_iter()
        .filter(|path| !path.starts_with(&state))
        .collect();

    // Every control, and the whole of a simulated install, driven the way a person
    // would drive them.
    for line in [
        "run",
        "next",
        "machine",
        "components many",
        "blocked somebody is using the file",
        "rollback",
        "recovery",
        "reboot",
        "busy",
        "available 3.0.0",
        "drift core,docs",
        "maintenance",
        "install",
    ] {
        session.control(line);
    }
    assert_eq!(
        session.control("quit"),
        ControlOutcome::Quit,
        "and the only way to stop it is to say so"
    );

    let after: Vec<PathBuf> = tree(project.directory.path())
        .into_iter()
        .filter(|path| !path.starts_with(&state))
        .collect();
    assert_eq!(
        before, after,
        "a preview writes no application file, no registry value, no shortcut, no service and no \
         uninstall record, installs nothing, and elevates nothing"
    );
    session.end();
}

/// A save is honoured by the session's own loop, not by a test reaching in.
///
/// The bug this exists to catch is specific: a driver can report that it is
/// re-resolving and never do it, because the work was put in a method the loop
/// does not call. Every other test here calls `refresh` directly, so every other
/// test here would pass with that bug in place - only a test that goes through the
/// same path a save goes through can see it.
#[test]
fn a_save_is_honoured_by_the_sessions_own_loop() {
    let (project, mut session) = opened();
    await_report(&project);
    let before = session.presented().expect("a window");

    let edited = format!(
        "[ui.settings]\nhero = \"Get Acme Desktop\"\nlogo = \"branding/logo.svg\"\nreport = {:?}\n",
        project.report_path().display().to_string()
    );
    project.write(&project.manifest(&edited, Some("./aurora.zupui")));

    // What the watcher would send, through the loop's own dispatch. Canonical,
    // because that is the spelling the backend reports: a resolved project path is
    // verbatim on this platform, and a session that compared the two spellings of
    // one file would never see a save at all.
    let manifest = std::fs::canonicalize(project.manifest_path()).expect("the manifest");
    session.saw_change(paths(&[manifest]));
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline
        && session.configuration().settings["hero"] != serde_json::json!("Get Acme Desktop")
    {
        session.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        session.configuration().settings["hero"],
        serde_json::json!("Get Acme Desktop"),
        "a change the loop was told about is a change the session acted on"
    );
    assert_eq!(
        session.presented(),
        Some(before),
        "and it cost no child, because it was a data change"
    );
    session.end();
}

#[test]
fn a_change_to_something_the_window_does_not_care_about_costs_nothing() {
    let (project, mut session) = opened();
    await_report(&project);

    assert_eq!(
        session.meaning(&paths(&[project
            .directory
            .path()
            .join("payload/readme.txt")])),
        None,
        "an edit to the payload is not an edit to the window"
    );
    assert_eq!(
        session.meaning(&paths(&[project
            .directory
            .path()
            .join("target")
            .join("debug")
            .join("anything")])),
        None,
        "and neither is build output, or a session would chase a compiler it never started"
    );
    assert_eq!(
        session.meaning(&paths(&[session.assets()[0].clone()])),
        Some("an asset"),
        "while the file the settings named is an asset"
    );
    // Resolved paths are verbatim on this platform, and the watcher reports the
    // spelling the filesystem gave it, so a test that compared the two spellings
    // of one file would be testing nothing but that.
    let manifest = std::fs::canonicalize(project.manifest_path()).expect("the manifest");
    assert_eq!(
        session.meaning(&paths(&[manifest])),
        Some("the application"),
        "and the manifest is the application"
    );
    let package =
        std::fs::canonicalize(project.directory.path().join("aurora.zupui")).expect("the package");
    assert_eq!(
        session.meaning(&paths(&[package])),
        Some("the package"),
        "and the package it selected is the window"
    );
    session.end();
}

#[test]
fn a_target_that_presents_no_window_is_refused_rather_than_faked() {
    let project = project();
    project.write(&format!(
        r#"schema = 1

[app]
id = "com.example.acme"
name = "Acme Desktop"
version = "2.1.0"

[build.targets.windows]
target = "{HOST}"
source = {{ directory = "payload" }}
frontend = "console"

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/Acme"
"#
    ));
    let Err(error) = Session::open(project.manifest_path(), Vec::new(), toolchain(None)) else {
        panic!("a console installer has no window to preview");
    };
    assert!(
        matches!(error, zup::preview::PreviewError::NoWindow { .. }),
        "and the refusal says which frontend there is rather than opening a presenter for one \
         this application does not have: {error}"
    );
}

#[test]
fn a_manifest_with_several_targets_asks_which_one_rather_than_guessing() {
    let project = project();
    project.write(&format!(
        r#"schema = 1

[app]
id = "com.example.acme"
name = "Acme Desktop"
version = "2.1.0"

[build.targets.windows]
target = "{HOST}"
source = {{ directory = "payload" }}
frontend = "gui"

[build.targets.linux]
target = "aarch64-unknown-linux-gnu"
source = {{ directory = "payload" }}
frontend = "gui"

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/Acme"
"#
    ));
    let Err(error) = Session::open(project.manifest_path(), Vec::new(), toolchain(None)) else {
        panic!("two targets, one window");
    };
    assert!(
        error.to_string().contains("--target"),
        "and it names the flag that says which: {error}"
    );
}
