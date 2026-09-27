//! A file a plugin generates, tracked as an owned resource for the whole
//! lifecycle.
//!
//! This is the one class of installed file the runtime did not receive. Every
//! other file arrives in the package; a generated file is produced while the
//! transaction is running, which makes three questions that a packaged file never
//! raises. Does the ledger claim it, so a later run can find and verify it? Does a
//! repair put it back after somebody deletes it? Does an uninstall remove it,
//! rather than leaving a file the application believes it wrote?
//!
//! The plugin source is deleted before anything runs. A build-time path that a
//! user's machine does not have must never become something a repair needs.

#![cfg(windows)]

use std::{fs, path::Path, thread, time::Duration};

use zup_bundle::{CompiledPluginArtifact, PluginArtifact};
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest, hash_reader};
use zup_exec::OwnedResource;
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, PluginEngine, WASMTIME_VERSION,
    wit_package_digest,
};
use zup_windows::{InstallLedgerStore, InstallationLock, PAYLOAD_OVERLAY_DIRECTORY};

#[path = "support/project.rs"]
mod project;

use project::{AppSpec, Payload, PluginSpec, State, compose_with, run, succeed};

const PLUGIN: &str = "configure";

/// The bytes the build hashed. They are not what ships, and the test deletes them
/// before anything runs to prove that.
const SOURCE_BYTES: &[u8] = b"configure component source is build-time only";

/// A precompiled plugin whose `plan` writes a file into the install directory.
///
/// The file's content is derived from the application identity and the resolved
/// install directory, which is what makes it worth tracking: it can only be
/// produced by running the plugin, and it has to be reproduced exactly.
fn precompiled(app_id: &str, install_name: &str) -> Vec<u8> {
    let contents = format!(
        "app id: {app_id}\ninstall directory: ${{location.user_data}}/{install_name}\nselected components: core\n"
    );
    let contents_len = contents.len();
    let wat = format!(
        r#"(module
        (type (func (param i32) (result i32)))
        (type (func (param i32)))
        (type (func (param i32 i32 i32 i32) (result i32)))
        (type (func))
        (memory (export "cm32p2_memory") 1)
        (func (export "cm32p2|zup:plugin/planner@1|plan") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
            (i32.store (i32.const 1024) (i32.const 0))
            (i32.store (i32.const 1028) (i32.const 2048))
            (i32.store (i32.const 1032) (i32.const 1))
            (i32.store (i32.const 2048) (i32.const 0))
            (i32.store (i32.const 2052) (i32.const 5000))
            (i32.store (i32.const 2056) (i32.const 28))
            (i32.store (i32.const 2060) (i32.const 6000))
            (i32.store (i32.const 2064) (i32.const {contents_len}))
            (i32.const 1024)
        )
        (func (export "cm32p2|zup:plugin/planner@1|plan_post") (param i32))
        (func (export "cm32p2_realloc") (param i32 i32 i32 i32) (result i32)
            local.get 0
        )
        (func (export "cm32p2_initialize"))
        (data (i32.const 5000) "${{install}}/plugin-config.txt")
        (data (i32.const 6000) "app id: {app_id}\0ainstall directory: ${{location.user_data}}/{install_name}\0aselected components: core\0a")
    )"#,
    );
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str(
            "zup-plugin.wit",
            include_str!("../../../wit/zup-plugin.wit"),
        )
        .expect("the plugin world parses");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("the plugin world is selectable");
    let mut module = wat::parse_str(wat).expect("the fixture module parses");
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .expect("the module carries component metadata");
    let component = wit_component::ComponentEncoder::default()
        .module(&module)
        .expect("the module encodes")
        .validate(true)
        .encode()
        .expect("the component encodes");
    PluginEngine::new(HOST_TARGET)
        .expect("a plugin engine")
        .precompile_component(&component)
        .expect("the component precompiles")
}

/// The compiled artifact the package carries for one application.
fn artifact(app_id: &str, install_name: &str) -> CompiledPluginArtifact {
    let bytes = precompiled(app_id, install_name);
    let engine = PluginEngine::new(HOST_TARGET).expect("a plugin engine");
    engine
        .verify_precompiled(&bytes)
        .expect("the fixture is a real precompiled component");
    let (source_size, source_sha256) = hash_reader(SOURCE_BYTES).expect("the source hashes");
    let (aot_size, aot_sha256) = hash_reader(bytes.as_slice()).expect("the artifact hashes");
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: zup_core::PluginId::new(PLUGIN).expect("a plugin id"),
            source_size,
            source_sha256,
            target: zup_core::TargetTriple::parse(HOST_TARGET).expect("a target"),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(*engine.fingerprint().as_bytes()),
            aot_size,
            aot_sha256,
            blob: aot_sha256,
        },
        bytes,
    )
    .expect("the artifact is well formed")
}

/// The content the fixture plugin writes, which the test can state without running
/// anything.
fn expected(app_id: &str, install_name: &str) -> String {
    format!(
        "app id: {app_id}\ninstall directory: ${{location.user_data}}/{install_name}\nselected components: core\n"
    )
}

/// Wait for an uninstall's detached cleanup to finish, rather than assuming it is
/// synchronous.
///
/// The uninstaller runs out of process so it can outlive the file it is deleting.
/// A test that asserted immediately would be asserting that a background process
/// had already been scheduled, which is not a property anybody promised.
fn wait_for_uninstall(state: &State, app_id: &AppId, generated: &Path) {
    for _ in 0..100 {
        let ledger_gone = InstallLedgerStore::new(state.path())
            .load(app_id, SelectedScope::User)
            .ok()
            .flatten()
            .is_none();
        let maintenance_gone = !state
            .path()
            .join("maintenance")
            .join(app_id.as_str())
            .exists();
        if ledger_gone && !generated.exists() && maintenance_gone {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the uninstall did not finish");
}

#[test]
fn a_generated_file_survives_the_whole_lifecycle_without_its_build_time_source() {
    let state = State::new();
    let app = AppSpec::unique("generated");
    let _cleanup = project::cleanup(&app, &state);
    let install = app.install_directory();
    let generated = install.join("plugin-config.txt");
    let app_id_text = app.id.clone();
    let install_name = app
        .install_directory
        .rsplit('/')
        .next()
        .expect("an install directory name")
        .to_owned();

    // The build-time source, hashed into the plan and then thrown away.
    let source_root = tempfile::TempDir::new().expect("a plugin source directory");
    let source_path = source_root.path().join("configure.component.wasm");
    fs::write(&source_path, SOURCE_BYTES).expect("the plugin source is written");
    let plugin = PluginSpec::new(PLUGIN, &source_path);
    let artifact = artifact(&app_id_text, &install_name);
    let payload = vec![Payload::named("app.exe", b"app payload", Some("core"))];

    let v1 = compose_with(
        &app,
        zup_core::Frontend::Gui,
        &payload,
        std::slice::from_ref(&plugin),
        std::slice::from_ref(&artifact),
    );
    let next = app.at_version("1.1.0");
    let v2 = compose_with(
        &next,
        zup_core::Frontend::Gui,
        &payload,
        std::slice::from_ref(&plugin),
        std::slice::from_ref(&artifact),
    );

    // The plan records where the plugin source was, and that path is not here any
    // more. Nothing downstream is allowed to need it.
    fs::remove_file(&source_path).expect("the plugin source is deletable");
    assert!(!source_path.exists());
    let plan = zup_windows::EmbeddedBundle::open(v1.path())
        .expect("the installer opens")
        .build_plan()
        .expect("the plan is readable");
    assert!(
        plan.targets[0].plugins.is_empty(),
        "the build plan inside the package carries no build-time plugin sources"
    );

    let app_id = AppId::new(&app_id_text).expect("an app id");
    let want = expected(&app_id_text, &install_name);
    let maintenance_v1 = state.maintenance(&app);
    let maintenance_v2 = state.maintenance(&next);

    succeed(v1.path(), &state, "install", &[]);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);
    assert!(maintenance_v1.is_file());

    // The ledger claims the generated file as an owned resource, or a repair would
    // have no way to know it exists.
    let ledger = InstallLedgerStore::new(state.path())
        .load(&app_id, SelectedScope::User)
        .expect("the ledger is readable")
        .expect("the application is installed");
    assert_eq!(ledger.version.to_string(), "1.0.0");
    let key = ResourceKey::File {
        destination: generated.to_string_lossy().into_owned(),
    };
    let OwnedResource::File {
        source_relative,
        sha256,
        size,
        ..
    } = ledger
        .resources
        .get(&key)
        .expect("the generated file is owned")
    else {
        panic!("the generated file is not owned as a file");
    };
    assert!(zup_windows::is_plugin_payload_path(source_relative));
    assert_eq!(*sha256, hash_reader(want.as_bytes()).expect("hashes").1);
    assert_eq!(*size, want.len() as u64);
    assert_eq!(
        ledger
            .resources
            .values()
            .filter(|resource| {
                matches!(
                    resource,
                    OwnedResource::File { source_relative, .. }
                        if zup_windows::is_plugin_payload_path(source_relative)
                )
            })
            .count(),
        1,
        "the generated file is claimed once"
    );

    // The downloaded setup is not part of the installation.
    drop(fs::remove_file(v1.path()));
    succeed(v2.path(), &state, "install", &[]);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);
    assert!(maintenance_v2.is_file());
    assert!(!maintenance_v1.exists());

    // A repair puts a deleted generated file back, and a repair that finds a
    // corrupted one needs to be told to overwrite rather than treat it as
    // somebody's edit.
    fs::remove_file(&generated).expect("the generated file is deletable");
    succeed(&maintenance_v2, &state, "repair", &[]);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);
    fs::write(&generated, b"corrupt").expect("the generated file is writable");
    succeed(&maintenance_v2, &state, "repair", &["--force-files"]);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);

    succeed(&maintenance_v2, &state, "uninstall", &[]);
    wait_for_uninstall(&state, &app_id, &generated);
    assert!(!generated.exists());
    assert!(!state.path().join("maintenance").join(&app_id_text).exists());
    for residue in ["transactions", "work", "installations"] {
        assert!(
            !state.path().join(residue).exists(),
            "{residue} survived the uninstall"
        );
    }
    let lock_key = InstallationLock::lock_key(&app_id_text, "user");
    assert!(!state.path().join(format!("{lock_key}.lock")).exists());
}

/// The plugin payload overlay is a scratch area. A file left there after a
/// successful run is a file a later transaction has to reason about for no reason.
#[test]
fn a_successful_run_leaves_no_payload_overlay_behind() {
    let state = State::new();
    let app = AppSpec::unique("overlay");
    let _cleanup = project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"app payload", Some("core"))];
    let setup = project::compose(&app, zup_core::Frontend::Gui, &payload);

    succeed(setup.path(), &state, "install", &[]);
    assert!(
        !state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists(),
        "a completed install left its scratch area behind"
    );

    let maintenance = state.maintenance(&app);
    succeed(&maintenance, &state, "uninstall", &[]);
    assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());
}

/// A failed transaction must not leave a half-written installation a later run
/// cannot tell from a complete one.
#[test]
fn an_uninstall_of_an_application_that_was_never_installed_is_a_refusal() {
    let state = State::new();
    let app = AppSpec::unique("absent");
    let _cleanup = project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"app payload", Some("core"))];
    let setup = project::compose(&app, zup_core::Frontend::Headless, &payload);

    let refused = run(setup.path(), &state, "uninstall", &["--yes"]);
    assert!(
        !refused.status.success(),
        "uninstalling something that is not installed must not report success"
    );
    assert!(!app.install_directory().exists());
}
