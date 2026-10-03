//! The preview runtime, driven the way a person drives it.
//!
//! The claims worth testing here are behavioural and about processes: a preset
//! that will not start must leave the working one alone, the state must survive a
//! child being replaced, and nothing a simulated install does may reach past the
//! session's own directory. Everything below drives the real state machine, the
//! real transport, and a real preset process launched from a real executable;
//! nothing is stubbed, because a stubbed child proves nothing about the handshake.
//!
//! The fixture preset is deliberately small. It speaks the production protocol
//! over the production transport and runs the production state machine, and it
//! does not open a window - the only thing between "the host launched a preset"
//! and "a person sees something" is a display, and a test that needs one proves
//! less than the one that does not. What it reports is what it was given, which
//! is how a test in this process finds out what a child in another one saw.
//!
//! It writes that report beside its own copy of the executable, because the
//! runtime stages each generation into a directory of its own and that is the one
//! place a child can name that two concurrent sessions will not share.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use zup_preview::{
    Command as Control, Components, Runtime, Scenario, StateDirectory, Surface, Watcher,
    default_scenario,
};

/// The child that answers, asks, and holds its session open.
const PRESET: &str = r##"
use std::path::PathBuf;
use zup_preset_ipc::Bootstrap;
use zup_preset_protocol::{
    Session, SessionProgress, SessionState, Action, Capabilities, Configuration, Message,
    SessionId,
};

/// Beside this executable, which the runtime staged into a directory of its own.
fn report_path() -> PathBuf {
    beside("REPORT")
}

/// A file beside this executable, which the runtime staged into a directory of
/// its own - so a helper it spawns writes where only this generation's tests
/// look.
fn beside(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("a launched executable");
    path.set_file_name(name);
    path
}

/// The role that outlives its parent unless the whole tree is ended.
///
/// It appends to a file forever and nothing else, which is the only observable a
/// test in another process needs: a heartbeat still growing is a tree still
/// running, and a heartbeat that has stopped is a tree that was ended. It cannot
/// be faked by the parent reporting on itself, which is the point - a preset that
/// says it is alive while its helpers are not has told the host nothing.
fn grandchild() -> ! {
    let heartbeat = beside("HEARTBEAT");
    loop {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&heartbeat)
        {
            let _ = writeln!(file, "alive");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Start a helper in this preset's own tree, and record that one was asked for.
///
/// Driven by a marker file beside the executable rather than by an argument,
/// because the host passes exactly one argument and a fixture that needed a
/// second would be testing a launch protocol zup does not have.
fn spawn_helper() {
    std::process::Command::new(std::env::current_exe().expect("a launched executable"))
        .arg("--zup-grandchild")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the helper starts");
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--zup-grandchild") {
        grandchild();
    }
    if arguments.iter().any(|argument| argument == "--zup-describe") {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "hero": { "type": "string" } },
            "additionalProperties": false,
        });
        let description =
            zup_preset_protocol::PresetDescription::new("probe", env!("CARGO_PKG_VERSION"), schema);
        println!("{}", serde_json::to_string(&description).expect("a description"));
        return;
    }

    let bootstrap = Bootstrap::from_arguments(arguments).expect("launched by a host");
    let channel = bootstrap.collect().expect("the transport opens");

    // The tree this preset owns, before anything else: a helper that has to be
    // ended with it, and a mode where this preset never finishes its handshake.
    if beside("SPAWNS").exists() {
        spawn_helper();
    }
    if beside("SILENT").exists() {
        // Collected the endpoint, then said nothing. The host is left waiting on a
        // handshake that will not complete, which is the case where it has to end
        // the tree itself rather than expect the preset to.
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    let sender = channel.sender().clone();
    let mut session = Session::preset(
        SessionId(channel.session()),
        Capabilities::default(),
        "probe".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    let hello = session.opening().expect("a hello");
    sender.send(&hello).expect("the host receives the hello");

    // The configuration and the snapshot are two frames, and the configuration
    // arrives first, so the session is not live until both have been read. The
    // report is rewritten on every publish, because a host may replace what it
    // configures while a session runs and a preset has to be able to see that.
    let mut configuration = None;
    let mut asked = false;
    loop {
        let envelope = channel.recv().expect("the host publishes");
        match session.receive(envelope.clone()).expect("a valid frame") {
            SessionProgress::Send(answer) => {
                sender.send(&answer).expect("the host receives the answer");
            }
            _ => match envelope.message {
                Message::Configuration(value) => configuration = Some(value),
                _ => {}
            },
        }
        let SessionState::Live { snapshot } = session.state() else {
            continue;
        };
        let configuration = configuration.clone().expect("a configuration arrives first");
        report(snapshot, &configuration);
        if asked {
            continue;
        }
        // One ask, so a test in this process can prove an action crossed the
        // transport rather than that a local value changed.
        asked = true;
        let frame = session
            .frame(Message::Action(Action::SetScope {
                scope: zup_preset_protocol::InstallScope::Machine,
            }))
            .expect("a framed action");
        sender.send(&frame).expect("the host receives the action");
    }
}

/// What the child was told, written where the test that launched it can read it.
fn report(snapshot: &zup_preset_protocol::Snapshot, configuration: &Configuration) {
    let mut seen = String::new();
    seen.push_str(&format!("name={}\n", snapshot.product.name));
    seen.push_str(&format!("version={}\n", snapshot.product.version));
    seen.push_str(&format!(
        "publisher={}\n",
        snapshot.product.publisher.clone().unwrap_or_default()
    ));
    seen.push_str(&format!(
        "description={}\n",
        snapshot.product.description.clone().unwrap_or_default()
    ));
    seen.push_str(&format!("surface={:?}\n", snapshot.surface));
    seen.push_str(&format!("state={:?}\n", snapshot.state));
    seen.push_str(&format!(
        "components={}\n",
        snapshot
            .surface
            .components()
            .iter()
            .map(|component| format!("{}:{}", component.id, component.name))
            .collect::<Vec<_>>()
            .join(",")
    ));
    seen.push_str(&format!(
        "settings={}\n",
        serde_json::to_string(&configuration.settings).expect("settings are readable")
    ));
    // The bytes, not the path: what a preset is promised about an asset is the
    // content, and a path would only prove the host can name a file.
    for (name, path) in &configuration.assets {
        let bytes = std::fs::read(path).unwrap_or_default();
        seen.push_str(&format!("asset {name}={}", String::from_utf8_lossy(&bytes)));
        seen.push('\n');
    }
    std::fs::write(report_path(), seen.as_bytes()).expect("the preset can report");
}
"##;

const PROBE_MANIFEST: &str = r#"
[package]
name = "probe"
version = "0.1.0"
edition = "2024"
publish = false

[[bin]]
name = "probe"
path = "src/main.rs"

[dependencies]
zup-preset-ipc = { path = "zup_preset_ipc", version = "0.1.0" }
zup-preset-protocol = { path = "zup_preset_protocol", version = "0.1.0" }
serde_json = "1"

[workspace]
"#;

fn crate_directory(name: &str) -> String {
    // Forward slashes, because a Windows path in a TOML string is a sequence of
    // escape sequences and a preset project's manifest is not the place to be
    // surprised by one.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace crates")
        .join(name)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Where the fixture preset is built, shared by every process that runs this
/// suite.
///
/// One fixed directory rather than one temporary directory per process, and the
/// reason is size rather than taste: this suite runs under `nextest`, which gives
/// each test its own process, and each process building its own copy of a preset
/// means a separate multi-gigabyte target directory per test. Nineteen tests left
/// behind nineteen of them, and the machine ran out of disk partway through the
/// workspace run.
///
/// Cargo takes a lock on a target directory, so concurrent processes here queue
/// rather than collide: the first builds, and the rest find it fresh and finish in
/// the time it takes to read a manifest. The build product is the same bytes either
/// way, which is all these tests ask of it.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .join("target")
        .join("zup-preview-fixture")
}

/// Cargo, configured the way this suite builds its own fixtures.
///
/// A resolver of its own so the fixture never joins the workspace it is built
/// from, and offline so a fixture build cannot reach for the network. The target
/// directory is the shared one above.
///
/// Deliberately a plain `Command`: this is a build that runs to completion and is
/// waited on before the test proceeds, so nothing outlives it to be terminated as
/// a group. The same command in `zup ui dev` is managed, because there a build is
/// something a session stops waiting for.
fn fixture_build(root: &Path) -> Command {
    let cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("CARGO_NET_OFFLINE", "true")
        .args(["build", "--message-format=json"]);
    command
}

/// The fixture preset, built once for the whole run.
///
/// Once per process, and cheap for every process after the first, because the
/// directory is shared: these tests are about behaviour after a build, and a nested
/// cargo per test would be a second build inside a build.
fn built() -> &'static PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let root = fixture_root();
        std::fs::create_dir_all(root.join("src")).expect("a source directory");
        std::fs::write(root.join("src/main.rs"), PRESET).expect("the preset source");
        std::fs::write(
            root.join("Cargo.toml"),
            PROBE_MANIFEST
                .replace("zup_preset_ipc", &crate_directory("zup-preset-ipc"))
                .replace("zup_preset_protocol", &crate_directory("zup-preset-protocol")),
        )
        .expect("the manifest");

        let output = fixture_build(&root).output().expect("cargo runs");
        assert!(
            output.status.success(),
            "the probe preset builds: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        executable_of(&output.stdout, "probe").expect("cargo named the executable")
    })
}

/// One executable out of a Cargo JSON stream, by binary name.
fn executable_of(stdout: &[u8], name: &str) -> Option<PathBuf> {
    for line in stdout.split(|byte| *byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" || message["target"]["name"] != name {
            continue;
        }
        if !message["target"]["kind"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        {
            continue;
        }
        if let Some(executable) = message["executable"].as_str() {
            return Some(PathBuf::from(executable));
        }
    }
    None
}

/// The bytes of the fixture preset.
fn preset_bytes() -> Vec<u8> {
    std::fs::read(built()).expect("the build product is readable")
}

/// How long a person - or a test - waits for a tree to start or to stop.
const PATIENCE: Duration = Duration::from_secs(30);

/// The heartbeat a generation's helper writes, if that generation spawned one.
fn heartbeat(state: &StateDirectory, generation: u64) -> PathBuf {
    run_directory(state, generation).join("HEARTBEAT")
}

/// One generation's staged directory, where its executable and its markers live.
fn run_directory(state: &StateDirectory, generation: u64) -> PathBuf {
    state.root().join("runs").join(generation.to_string())
}

/// Tell the next staged generation to behave a certain way.
///
/// Markers rather than arguments, because the host passes a preset exactly one
/// argument and a fixture that needed another would be asserting a launch
/// protocol zup does not have. The marker sits beside the executable the runtime
/// stages, which is the same place the preset looks.
fn mark(state: &StateDirectory, generation: u64, marker: &str) {
    let directory = run_directory(state, generation);
    std::fs::create_dir_all(&directory).expect("the generation's directory");
    std::fs::write(directory.join(marker), b"").expect("the marker is written");
}

/// Whether a heartbeat is still being written, sampled twice.
///
/// A heartbeat file that exists is not evidence: a helper writes it before it is
/// killed too. What proves a tree is running is that the file is still growing
/// between two reads, and what proves it was ended is that it has stopped.
fn beating(path: &Path) -> bool {
    let size = |path: &Path| std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let first = size(path);
    if first == 0 {
        return false;
    }
    std::thread::sleep(Duration::from_millis(400));
    size(path) > first
}

/// Wait for a helper to be running, and fail rather than assert on a tree that
/// never started.
fn await_beating(path: &Path) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if beating(path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "the helper never reported that it is running: {}",
        path.display()
    );
}

/// Wait for a tree to be gone, and fail rather than assert on one that is still
/// running.
///
/// Generous in both directions: a heartbeat that stops is a claim about a process
/// this process cannot schedule, so it is given time to be true. A test that gave
/// up early would pass on a leak, which is the failure this exists to catch.
fn await_still(path: &Path) {
    std::thread::sleep(Duration::from_millis(500));
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if !beating(path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "a helper is still running after its owner ended: {}",
        path.display()
    );
}

/// A session's own directory, per test.
fn scratch() -> (tempfile::TempDir, StateDirectory) {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let state = StateDirectory::under(directory.path(), "preview");
    (directory, state)
}

/// The preset the session is told about, described the way the SDK describes one.
fn preset() -> zup_core::UiPreset {
    zup_core::UiPreset {
        name: zup_core::NonEmptyString::new("probe").expect("a name"),
        version: semver::Version::parse("0.1.0").expect("a version"),
        protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
        required_capabilities: Vec::new(),
        settings: serde_json::json!({ "hero": "first" }),
        assets: Vec::new(),
    }
}

fn runtime(state: StateDirectory) -> Runtime {
    Runtime::new(state, default_scenario())
}

/// Present the fixture, which is only possible once it has opened its session.
fn present(runtime: &mut Runtime) {
    let generation = runtime
        .present(&preset(), &preset_bytes())
        .expect("the fixture opens a session");
    assert!(generation > 0, "and it is a generation of its own");
    assert!(
        runtime.is_running(),
        "and the child is the one that is running"
    );
}

/// Present the fixture and wait until its action has been through the host.
///
/// The evidence is the state rather than the action: the child asks to change
/// scope, and the host applying it is the only way the snapshot's scope becomes
/// something the child chose. That proves the action crossed the transport and
/// was validated by the state machine, which is the whole claim.
fn present_and_wait_for_the_ask(runtime: &mut Runtime) {
    present(runtime);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        runtime.drain_actions();
        if runtime.snapshot().surface.scope() == zup_preset_protocol::InstallScope::Machine {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the child asked to change scope and the host never saw it");
}

/// Every path under `root`, as it was, so a test can prove nothing was written.
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

#[test]
fn a_preset_reaches_the_runtime_over_the_production_transport() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    present_and_wait_for_the_ask(&mut runtime);
    assert_eq!(
        runtime.snapshot().surface.scope(),
        zup_preset_protocol::InstallScope::Machine,
        "an action a real child sent over the real transport was validated by the state machine \
         and published back"
    );
    runtime.shutdown();
}

#[test]
fn the_state_a_replaced_child_connects_to_is_the_state_the_host_owns() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    present(&mut runtime);
    // Put the simulated machine halfway through an installation, which is the
    // state a replacement has to preserve.
    runtime
        .simulator_mut()
        .act(zup_preset_protocol::Action::Install);
    for event in zup_runtime_events() {
        runtime.simulator_mut().observe(&event);
    }
    let before = runtime.snapshot().clone();
    let percent = before
        .progress
        .as_ref()
        .expect("an operation is running")
        .percent();
    assert!(percent.is_some(), "and it reports a position");

    present_and_wait_for_the_ask(&mut runtime);
    let after = runtime.snapshot();
    assert_eq!(
        after.state, before.state,
        "the replaced child reconnects to the state it was in, not to a fresh one"
    );
    assert_eq!(
        after
            .progress
            .as_ref()
            .and_then(|progress| progress.percent()),
        percent,
        "including how far along it is"
    );
    runtime.shutdown();
}

#[test]
fn a_replacement_generation_is_a_file_of_its_own() {
    let (directory, state) = scratch();
    let mut runtime = runtime(state);
    present_and_wait_for_the_ask(&mut runtime);
    present(&mut runtime);

    let runs = directory.path().join(".zup").join("preview").join("runs");
    let generations: Vec<String> = std::fs::read_dir(&runs)
        .expect("the runs directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        generations.iter().any(|name| name == "1") && generations.iter().any(|name| name == "2"),
        "each generation is a directory of its own, because a running executable cannot be \
         overwritten: {generations:?}"
    );
    runtime.shutdown();
}

#[test]
fn an_asset_change_is_a_new_file_at_a_new_address() {
    let (directory, state) = scratch();
    let root = state.root().to_path_buf();
    let mut runtime = runtime(state);
    let source = directory.path().join("logo.svg");
    std::fs::write(&source, b"<svg/>").expect("the first version");
    let first = runtime
        .set_asset("logo", &source)
        .expect("the asset is materialized");
    let first_path = runtime.configuration().assets["logo"].clone();

    std::fs::write(&source, b"<svg width='8'/>").expect("the second version");
    let second = runtime
        .set_asset("logo", &source)
        .expect("the asset is materialized again");
    assert_ne!(first, second, "changed content is different content");
    assert_ne!(
        first_path,
        runtime.configuration().assets["logo"],
        "and it is served from a different place, so nothing can be looking at the old one"
    );
    assert!(
        Path::new(&first_path).starts_with(&root),
        "and the file a preset is given lives inside the session's own directory, never at a path \
         the project wrote: {}",
        first_path
    );
}

#[test]
fn a_surface_change_reopens_the_machine_without_closing_the_window() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    present_and_wait_for_the_ask(&mut runtime);
    runtime.simulator_mut().reopen(&Scenario {
        surface: Surface::Maintenance,
        ..default_scenario()
    });
    assert!(
        matches!(
            runtime.snapshot().surface,
            zup_preset_protocol::Surface::Maintenance(_)
        ),
        "the machine is a different one"
    );
    assert!(
        runtime.is_running(),
        "and the child is still up, because the settings and the files it was given are still the \
         ones it has"
    );
    runtime.shutdown();
}

#[test]
fn a_preset_that_needs_something_this_machine_cannot_do_is_never_launched() {
    let (_directory, state) = scratch();
    let runtime = runtime(state);
    let mut demanding = preset();
    demanding.protocol = zup_preset_protocol::PRESET_PROTOCOL_VERSION + 1;
    let error = runtime
        .simulator()
        .stage(&preset_bytes(), &demanding)
        .expect_err("a preset this host cannot present");
    assert!(
        error.to_string().contains("cannot present"),
        "and it says so rather than launching something that will be refused: {error}"
    );
}

#[test]
fn shutdown_ends_the_child_rather_than_leaving_it_holding_its_executable() {
    let (directory, state) = scratch();
    let mut runtime = runtime(state);
    present(&mut runtime);
    let executable = directory
        .path()
        .join(".zup")
        .join("preview")
        .join("runs")
        .join("1");
    runtime.shutdown();
    assert!(executable.is_dir(), "the generation is still on disk");
    // The point of the assertion: the file can be replaced. A child that was not
    // reaped would still hold it open, and on this platform that is a file the
    // next generation could not be written to.
    std::fs::write(executable.join("probe.exe"), b"replaced").expect("the file is writable again");
}

/// The tree a preset started ends with it, on every platform.
///
/// The witness is a helper the preset spawned, not the preset's own claim about
/// itself: a preset that reports itself alive while its helpers are still running
/// has told this process nothing, and on both supported platforms that is the
/// failure a host without process-tree semantics produces.
#[test]
fn ending_a_preset_ends_the_tree_it_started() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state.clone());
    mark(&state, 1, "SPAWNS");
    present(&mut runtime);
    let helper = heartbeat(&state, 1);
    await_beating(&helper);

    runtime.shutdown();
    await_still(&helper);
}

/// The same guarantee by the other route: an owner that is dropped rather than
/// stopped. A session that ends because it went out of scope must leave nothing
/// behind for the same reason one that ends deliberately does.
#[test]
fn dropping_the_session_ends_the_tree_it_started() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state.clone());
    mark(&state, 1, "SPAWNS");
    present(&mut runtime);
    let helper = heartbeat(&state, 1);
    await_beating(&helper);

    drop(runtime);
    await_still(&helper);
}

/// A launch that does not complete ends what it started.
///
/// The preset collected the endpoint and then said nothing, so the handshake never
/// finishes and the host is left holding a child it will never talk to. The
/// guarantee is that it does not go on holding it: the tree is ended at the point
/// the failure is reported, rather than becoming a preset with no host and no one
/// to end it.
#[test]
fn a_preset_that_never_completes_its_handshake_leaves_no_orphan() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state.clone());
    mark(&state, 1, "SPAWNS");
    mark(&state, 1, "SILENT");

    let error = runtime
        .present(&preset(), &preset_bytes())
        .expect_err("the handshake never completes");
    assert!(
        error.to_string().contains("handshake"),
        "and it says what failed rather than reporting an empty success: {error}"
    );
    await_still(&heartbeat(&state, 1));
    assert!(
        runtime.generation().is_none(),
        "and no generation was adopted, because nothing ever opened a session"
    );
}

/// A replacement ends the old tree, and only the new generation survives.
///
/// Both halves matter, and they fail independently: a host that ended the new
/// child instead of the old one leaves a window that is not there, and a host
/// that did not end the old one leaves a window nobody is looking at - holding an
/// executable the next build would otherwise be unable to write.
#[test]
fn a_replacement_ends_the_old_tree_and_keeps_the_new_one_running() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state.clone());
    mark(&state, 1, "SPAWNS");
    present(&mut runtime);
    let first = heartbeat(&state, 1);
    await_beating(&first);

    // The replacement is marked too, so the new tree is observable and the claim is
    // about which of two live trees survives rather than about one.
    mark(&state, 2, "SPAWNS");
    present(&mut runtime);
    assert_eq!(
        runtime.generation(),
        Some(2),
        "the new generation is the one that is running"
    );
    let second = heartbeat(&state, 2);
    await_beating(&second);
    await_still(&first);

    runtime.shutdown();
    await_still(&second);
}

/// The replacement ordering, proved through what a preset observes.
///
/// The old tree has to survive the new child's handshake, or a preset that failed
/// to start would cost the person the window that was working. This is the same
/// rule from the other side: the state the old generation was showing is still
/// its state when it is ended.
#[test]
fn a_preset_that_will_not_start_leaves_the_running_one_alone() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state.clone());
    mark(&state, 1, "SPAWNS");
    present_and_wait_for_the_ask(&mut runtime);
    let working = heartbeat(&state, 1);
    await_beating(&working);

    // A file with the right name and nothing a program can do: it stages, and it
    // cannot open a session.
    let broken = b"this is not a program";
    assert!(
        runtime.present(&preset(), broken).is_err(),
        "the handshake fails"
    );
    assert!(
        runtime.generation() == Some(1) && runtime.is_running(),
        "and the child that was working is still the one that is running, because nothing above \
         touched it"
    );
    assert!(
        beating(&working),
        "and its tree is untouched with it: a replacement that starts by stopping the running child \
         costs a working window every time a build is wrong"
    );
    runtime.shutdown();
    await_still(&working);
}

#[test]
fn a_watcher_reports_a_save_and_ignores_what_the_session_wrote() {
    let (directory, state) = scratch();
    // The watcher blocks, which is what a session's watch thread wants, so the
    // reading runs on a thread of its own and the assertions wait with a
    // deadline. A test that called it directly would block on the first save and
    // never reach the second claim. Readiness is signalled separately, because a
    // save written before the watch exists is a save nobody was ever told about
    // and the test would be asserting a race rather than the behaviour.
    let (ready, watching) = std::sync::mpsc::channel();
    let (reported, seen) = std::sync::mpsc::channel();
    let root = directory.path().to_path_buf();
    let owned = state.clone();
    std::thread::spawn(move || {
        let mut watcher = Watcher::start(&root, owned).expect("it watches");
        if ready.send(()).is_err() {
            return;
        }
        while let Some(event) = watcher.next_change() {
            if reported.send(event).is_err() {
                return;
            }
        }
    });
    watching
        .recv_timeout(Duration::from_secs(30))
        .expect("the watch exists before anything is saved");

    let source = directory.path().join("zup.toml");
    std::fs::write(&source, b"[app]\n").expect("the manifest is saved");
    assert!(
        within(&seen, Duration::from_secs(30), |event| {
            matches!(event, zup_preview::Seen::Changed(_))
        }),
        "a save to the project is a change the session is told about"
    );

    // A file the session itself wrote must not come back, or a session would
    // replace the window it just started.
    let own = state.asset_directory(zup_core::hash_bytes(b"logo"));
    std::fs::create_dir_all(own.parent().expect("a parent")).expect("the state directory");
    std::fs::write(&own, b"x").expect("the session writes its own file");
    assert!(
        !within(&seen, Duration::from_millis(750), |event| {
            matches!(event, zup_preview::Seen::Changed(_))
        }),
        "and nothing the session materialized is reported back to it"
    );
    // The reader is not joined. It is blocked in the watcher's own `recv`, which
    // is what a session's watch thread does for as long as the session runs, and
    // a test that waited for it to stop would be waiting for a session to end.
    // Dropping the channel ends it at the next event, and the process ends it
    // otherwise.
    drop(seen);
}

/// Whether `wanted` arrives within `budget`.
fn within(
    seen: &std::sync::mpsc::Receiver<zup_preview::Seen>,
    budget: Duration,
    wanted: impl Fn(&zup_preview::Seen) -> bool,
) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match seen.recv_timeout(remaining) {
            Ok(event) if wanted(&event) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

#[test]
fn a_simulated_install_touches_nothing_outside_the_session_directory() {
    let (directory, state) = scratch();
    let root = state.root().to_path_buf();
    let mut runtime = runtime(state);
    present(&mut runtime);
    let before: Vec<PathBuf> = tree(directory.path())
        .into_iter()
        .filter(|path| !path.starts_with(&root))
        .collect();

    // Every control a person can reach, driven the way they would drive it.
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
        "available 2.0.0",
        "drift core,docs",
        "maintenance",
        "install",
    ] {
        runtime.control(line);
    }
    runtime.control("quit");

    let after: Vec<PathBuf> = tree(directory.path())
        .into_iter()
        .filter(|path| !path.starts_with(&root))
        .collect();
    assert_eq!(
        before, after,
        "a simulated install writes no application file, no registry value, no shortcut and no \
         service, and installs nothing"
    );
    runtime.shutdown();
}

#[test]
fn a_cancel_during_an_operation_leaves_the_machine_waiting_for_a_safe_boundary() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    present(&mut runtime);
    runtime
        .simulator_mut()
        .act(zup_preset_protocol::Action::Install);
    for event in zup_runtime_events() {
        runtime.simulator_mut().observe(&event);
    }
    assert!(
        runtime.snapshot().state.is_active(),
        "an operation is running"
    );
    runtime
        .simulator_mut()
        .act(zup_preset_protocol::Action::Cancel);
    assert_eq!(
        runtime.snapshot().state,
        zup_preset_protocol::InstallerState::WaitingForSafeCancellation,
        "and cancelling it puts the machine where a real one would be rather than pretending it \
         stopped"
    );
    runtime.shutdown();
}

#[test]
fn a_maintenance_action_asks_the_engine_rather_than_performing_anything() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    runtime.simulator_mut().reopen(&Scenario {
        surface: Surface::Maintenance,
        ..default_scenario()
    });
    present(&mut runtime);

    let decision = runtime
        .simulator_mut()
        .act(zup_preset_protocol::Action::Repair);
    assert!(
        matches!(decision, zup_preset_host::HostDecision::Run { .. }),
        "a repair is a request, and the preview has no engine to run it with"
    );
    assert_eq!(
        runtime.snapshot().state,
        zup_preset_protocol::InstallerState::Running,
        "so the machine is running, and the session is what starts the engine"
    );
    assert_eq!(
        runtime
            .snapshot()
            .progress
            .as_ref()
            .and_then(|progress| progress.percent()),
        None,
        "and it reports no position, because nothing has done any work: a progress bar a preview \
         invented would be a state a real install cannot be in"
    );
    assert!(
        runtime.snapshot().diagnostic.is_none(),
        "and no failure, because a request that was accepted has not failed"
    );
    runtime.shutdown();
}

#[test]
fn an_install_click_advances_the_simulated_lifecycle() {
    let (_directory, state) = scratch();
    let mut runtime = runtime(state);
    present(&mut runtime);
    assert_eq!(
        runtime.snapshot().state,
        zup_preset_protocol::InstallerState::Options,
        "a machine that has not had this application is waiting for a person"
    );

    runtime
        .simulator_mut()
        .act(zup_preset_protocol::Action::Install);
    for event in zup_runtime_events() {
        runtime.simulator_mut().observe(&event);
    }
    let snapshot = runtime.snapshot();
    assert!(
        snapshot.state.is_active(),
        "clicking Install starts a lifecycle, which is what the window would show"
    );
    assert_eq!(
        snapshot
            .progress
            .as_ref()
            .and_then(|progress| progress.percent()),
        Some(50),
        "and the progress the engine would have reported is the progress shown"
    );
    runtime.shutdown();
}

/// The events an install passes through, which the controls synthesise and a test
/// states directly.
fn zup_runtime_events() -> Vec<zup_runtime::RuntimeEvent> {
    vec![
        zup_runtime::RuntimeEvent::StateChanged {
            state: zup_runtime::RuntimeState::Preparing,
        },
        zup_runtime::RuntimeEvent::PreflightStarted,
        zup_runtime::RuntimeEvent::StagingStarted {
            id: "application".into(),
        },
        zup_runtime::RuntimeEvent::Progress {
            completed: 2_048,
            total: 4_096,
            action: "Writing application files".into(),
        },
    ]
}

/// A data change reaches a real child, and costs no child.
///
/// The witness is the child's own report rather than the host's configuration,
/// because a configuration the host holds proves only that the host took it. A
/// child in another process reading what it was sent is the whole claim, and it is
/// what makes a settings edit feel immediate rather than merely correct.
#[test]
fn a_data_change_reaches_a_real_child_without_replacing_it() {
    let (directory, state) = scratch();
    let root = state.root().to_path_buf();
    let project = directory.path().to_path_buf();
    let mut runtime = runtime(state);
    present_and_wait_for_the_ask(&mut runtime);
    let generation = runtime.generation().expect("a window");

    runtime
        .set_asset("logo", &write_asset(&project, "<svg width='4'/>", "one"))
        .expect("the asset is materialized");
    runtime.set_settings(serde_json::json!({ "hero": "first" }));
    let seen = await_report(&root, generation, |text| text.contains("hero\":\"first"));
    assert!(
        seen.contains(r#"settings={"hero":"first"}"#),
        "the child was given the settings over the ordinary session: {seen}"
    );
    assert!(
        seen.contains("asset logo=<svg width='4'/>"),
        "and the content of the file the application provided, not a path to it: {seen}"
    );

    runtime
        .set_asset("logo", &write_asset(&project, "<svg width='8'/>", "two"))
        .expect("the asset is materialized again");
    runtime.set_settings(serde_json::json!({ "hero": "second" }));
    let seen = await_report(&root, generation, |text| text.contains("hero\":\"second"));
    assert!(
        seen.contains(r#"settings={"hero":"second"}"#),
        "so a later change reaches the same child: {seen}"
    );
    assert!(
        seen.contains("asset logo=<svg width='8'/>"),
        "including the new bytes, from a new address: {seen}"
    );
    assert_eq!(
        runtime.generation(),
        Some(generation),
        "and the child is the same one, because a data change costs no compiler and no process"
    );
    runtime.shutdown();
}

/// The branding file a test changes, and a revision of it under a second name so a
/// re-materialization is a new address rather than an overwrite of the old one.
fn write_asset(project: &Path, contents: &str, revision: &str) -> PathBuf {
    let branding = project.join("branding");
    std::fs::create_dir_all(&branding).expect("a branding directory");
    let path = branding.join(format!("logo-{revision}.svg"));
    std::fs::write(&path, contents).expect("the branding file");
    path
}

/// The report a generation of a child wrote beside its own executable, once it has
/// reported what it was told.
fn await_report(state: &Path, generation: u64, wanted: impl Fn(&str) -> bool) -> String {
    let path = state
        .join("runs")
        .join(generation.to_string())
        .join("REPORT");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path)
            && wanted(&text)
        {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "generation {generation} never reported what it was told: {}",
        std::fs::read_to_string(&path).unwrap_or_default()
    );
}

/// The controls are one set, and both previews reach them through here.
#[test]
fn every_control_a_real_installation_has_a_shape_for_is_reachable() {
    for line in [
        "install",
        "maintenance",
        "user",
        "machine",
        "components none",
        "components one-optional",
        "components many",
        "components required-and-optional",
        "run",
        "next",
        "blocked a file is in the way",
        "rollback",
        "recovery",
        "reboot",
        "busy",
        "checking the channel",
        "up-to-date 1.4.0",
        "available 1.5.0",
        "update-failed no route",
        "drift core,docs",
        "quit",
    ] {
        assert!(
            Control::parse(line).is_ok(),
            "`{line}` is a control a person can press"
        );
    }
    assert_eq!(Components::Many.options().len(), 12);
}
