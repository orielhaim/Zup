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
use zup_core::PluginArtifact;

use zup_bundle::CompiledPluginArtifact;
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest, hash_reader};
use zup_plan::OwnedResource;
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, PluginEngine, WASMTIME_VERSION,
    wit_package_digest,
};
use zup_windows::{InstallationRecordStore, PAYLOAD_OVERLAY_DIRECTORY};

#[path = "support/project.rs"]
mod project;

use project::{AppSpec, Payload, PluginSpec, State, compose_with, run, succeed};

const PLUGIN: &str = "configure";

fn artifact() -> CompiledPluginArtifact {
    let source = zup_plugin_abi::test_fixture::component();
    let engine = PluginEngine::new(HOST_TARGET).expect("a plugin engine");
    let bytes = engine.precompile_component(&source).unwrap();
    let (source_size, source_sha256) = hash_reader(source.as_slice()).expect("the source hashes");
    let (aot_size, aot_sha256) = hash_reader(bytes.as_slice()).expect("the artifact hashes");
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: zup_core::PluginId::new(PLUGIN).expect("a plugin id"),
            source_size,
            source_sha256,
            target: zup_core::Target::parse(HOST_TARGET).expect("a target"),
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
fn expected(app: &AppSpec) -> String {
    format!(
        "application: {}\nversion: {}\nplugin: configure\ninstall directory: {}\nscope: user\nselected components: core\n",
        app.name,
        app.version,
        app.install_directory().display()
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
        let ledger_gone = InstallationRecordStore::new(state.path())
            .load(app_id, SelectedScope::User)
            .ok()
            .flatten()
            .is_none_or(|record| !record.is_installed());
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
    let generated = install.join("configure.txt");
    let app_id_text = app.id.clone();

    // The build-time source, hashed into the plan and then thrown away.
    let source_root = tempfile::TempDir::new().expect("a plugin source directory");
    let source_path = source_root.path().join("configure.component.wasm");
    fs::write(&source_path, zup_plugin_abi::test_fixture::component())
        .expect("the plugin source is written");
    let plugin = PluginSpec::new(PLUGIN, &source_path);
    let artifact = artifact();
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
    let want = expected(&app);

    succeed(v1.path(), &state, "install", &[]);
    let maintenance_v1 = state.maintenance(&app);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);
    assert!(maintenance_v1.is_file());

    // The ledger claims the generated file as an owned resource, or a repair would
    // have no way to know it exists.
    let ledger = InstallationRecordStore::new(state.path())
        .load(&app_id, SelectedScope::User)
        .expect("the ledger is readable")
        .expect("the application is installed");
    assert_eq!(ledger.current.version.to_string(), "1.0.0");
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Plugin {
            plugin: zup_core::PluginId::new(PLUGIN).unwrap(),
            id: zup_core::ResourceId::new("configuration").unwrap(),
        },
    };
    let OwnedResource::File {
        source_relative,
        sha256,
        size,
        ..
    } = ledger
        .current
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
            .current
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
    succeed(v2.path(), &state, "upgrade", &[]);
    let maintenance_v2 = state.maintenance(&next);
    let want = expected(&next);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);
    assert!(maintenance_v2.is_file());
    assert!(maintenance_v1.exists());

    fs::remove_file(&generated).expect("the generated file is deletable");
    succeed(&maintenance_v2, &state, "repair", &[]);
    assert_eq!(fs::read_to_string(&generated).unwrap(), want);

    succeed(&maintenance_v2, &state, "uninstall", &[]);
    wait_for_uninstall(&state, &app_id, &generated);
    assert!(!generated.exists());
    let removed = InstallationRecordStore::new(state.path())
        .load(&app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert!(!removed.is_installed());
    assert!(maintenance_v1.exists());
    assert!(maintenance_v2.exists());
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

#[test]
#[ignore = "exports installers for the isolated Windows recovery VM"]
fn export_machine_generated_recovery_fixture() {
    let output = std::path::PathBuf::from(
        std::env::var_os("ZUP_RECOVERY_FIXTURE_ROOT").expect("fixture output directory"),
    );
    fs::create_dir_all(&output).unwrap();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let app = AppSpec {
        id: format!("com.zup.f05-{token}"),
        name: "Zup F05 generated recovery".into(),
        version: "1.0.0".into(),
        install_directory: format!("${{location.programs}}/ZupF05-{token}"),
    };
    let source = tempfile::tempdir().unwrap();
    let plugin_path = source.path().join("configure.wasm");
    fs::write(&plugin_path, zup_plugin_abi::test_fixture::component()).unwrap();
    let plugin = PluginSpec::new(PLUGIN, &plugin_path);
    let artifact = artifact();
    for version in ["1.0.0", "2.0.0"] {
        let app = app.at_version(version);
        let payload = [Payload::named(
            "asset.bin",
            version.as_bytes(),
            Some("core"),
        )];
        let build_source = source.path().join(version);
        fs::create_dir(&build_source).unwrap();
        let mut plan = project::plan(
            &app,
            zup_core::Frontend::Headless,
            &payload,
            std::slice::from_ref(&plugin),
            &build_source,
        );
        plan.installer.install.scope = zup_core::InstallScope::Machine;
        plan.installer.install.directory = zup_core::InstallDirectory {
            user: None,
            machine: Some(zup_core::Template::parse(&app.install_directory).unwrap()),
        };
        plan.files[0].placement = zup_core::PayloadPlacement::Generation {
            path: zup_core::RelativePath::new("asset.bin").unwrap(),
        };
        plan.installer.files[0].placement = zup_core::PayloadMapping::GenerationFile {
            path: zup_core::RelativePath::new("asset.bin").unwrap(),
        };
        zup_windows::build_self_contained_executable(
            &project::template(zup_core::Frontend::Headless),
            &output.join(format!("setup-{version}.exe")),
            &plan,
            std::slice::from_ref(&artifact),
            None,
        )
        .unwrap();
    }
    fs::write(
        output.join("fixture.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "app": app.id,
            "install_folder": format!("ZupF05-{token}"),
        }))
        .unwrap(),
    )
    .unwrap();
}
