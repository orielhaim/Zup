#![cfg(windows)]

use std::path::{Path, PathBuf};

use zup_core::{
    AppId, InstalledPreset, NonEmptyString, PresetAsset, PresetRuntime, ResourceKey, SelectedScope,
    Sha256Digest, TargetTriple,
};
use zup_exec::InstallLedger;
use zup_transaction::{
    FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork, TransactionInput,
    TransactionPlan, compile_transaction,
};

fn zup_core_preset_protocol_version() -> u32 {
    1
}

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target")
}

fn app_id() -> AppId {
    AppId::new("com.zup.test-ui").expect("a valid id")
}

fn version() -> semver::Version {
    "1.0.0".parse().expect("a version")
}

fn target_path(path: &Path) -> zup_platform::TargetPath {
    zup_platform::TargetPath::new(target(), zup_windows::plain_path_text(path))
        .expect("a target path")
}

fn window(executable: &[u8], logo: &[u8]) -> InstalledPreset {
    InstalledPreset {
        preset: PresetRuntime {
            name: NonEmptyString::new("aurora").expect("a name"),
            version: "1.4.2".parse().expect("a version"),
            protocol: zup_core_preset_protocol_version(),
            required_capabilities: vec!["components".to_owned()],
            settings: serde_json::json!({
                "hero": "Install Acme",
                "logo": "branding/logo.svg",
                "nested": { "depth": [1, 2, 3], "on": true, "empty": {} },
            }),
            assets: vec![PresetAsset {
                name: NonEmptyString::new("branding/logo.svg").expect("a name"),
                size: logo.len() as u64,
                sha256: zup_core::hash_bytes(logo),
            }],
        },
        executable: zup_core::hash_bytes(executable),
    }
}

fn content_directory(
    state_root: &Path,
    scope: SelectedScope,
    version: &semver::Version,
) -> PathBuf {
    zup_transaction::maintenance_directory(state_root, &app_id(), scope, version)
}

fn content_files(
    state_root: &Path,
    scope: SelectedScope,
    ui: &InstalledPreset,
) -> Vec<(PathBuf, Sha256Digest)> {
    let directory = content_directory(state_root, scope, &version());
    let mut files = vec![(
        zup_bundle::preset_path(&directory, &ui.executable, target().executable_suffix()),
        ui.executable,
    )];
    files.extend(ui.preset.assets.iter().map(|asset| {
        (
            zup_bundle::asset_path(&directory, asset.name.as_str(), &asset.sha256),
            asset.sha256,
        )
    }));
    files
}

fn install_plan(state_root: &Path, scope: SelectedScope, ui: &InstalledPreset) -> TransactionPlan {
    let mut input = TransactionInput::new(target());
    for (path, sha256) in content_files(state_root, scope, ui) {
        input.files.push(FileWork {
            key: ResourceKey::File {
                destination: zup_windows::plain_path_text(&path),
            },
            source_relative: zup_core::RelativePath::new("content").expect("a relative path"),
            destination: target_path(&path),
            precondition: FilePrecondition::Absent,
            expected_sha256: sha256,
            expected_size: 1,
            privilege: scope.authorization(),
            delta: FileDelta::Create,
            executable: false,
        });
    }
    input.preset = Some(ui.clone());
    compile_transaction(&input).expect("the plan compiles")
}

fn retire_plan(state_root: &Path, scope: SelectedScope, ui: &InstalledPreset) -> TransactionPlan {
    let mut input = TransactionInput::new(target());
    for (path, sha256) in content_files(state_root, scope, ui) {
        let key = ResourceKey::File {
            destination: zup_windows::plain_path_text(&path),
        };
        input.retired_keys.push(key.clone());
        input.removals.push(FileRemoval {
            key,
            kind: FileRemovalKind::RemoveOwned,
            scope,
            privilege: scope.authorization(),
            destination: target_path(&path),
            sha256,
            size: 1,
            created_directories: Vec::new(),
        });
    }
    compile_transaction(&input).expect("the plan compiles")
}

fn validate(
    state_root: &Path,
    scope: SelectedScope,
    plan: &TransactionPlan,
    ledger: Option<InstallLedger>,
) -> Result<(), String> {
    let store = zup_windows::InstallLedgerStore::new(state_root);
    std::fs::create_dir_all(state_root.join("installations")).ok();
    match ledger {
        Some(ledger) => {
            let name = store.path_for(&app_id(), scope);
            std::fs::write(
                &name,
                serde_json::to_vec(&ledger).expect("the ledger serializes"),
            )
            .expect("the ledger is written");
        }
        None => {
            let name = store.path_for(&app_id(), scope);
            let _ = std::fs::remove_file(name);
        }
    }
    store
        .validate_plan(&app_id(), scope, &version(), plan)
        .map_err(|error| error.to_string())
}

#[test]
fn a_window_and_its_content_are_accepted_together() {
    let state_root = tempfile::tempdir().expect("a state root");
    let ui = window(b"a preset executable", b"<svg/>");
    validate(
        state_root.path(),
        SelectedScope::User,
        &install_plan(state_root.path(), SelectedScope::User, &ui),
        None,
    )
    .expect("a plan that installs the window it declares");
}

#[test]
fn a_window_whose_content_the_plan_does_not_install_is_refused() {
    let state_root = tempfile::tempdir().expect("a state root");
    let ui = window(b"a preset executable", b"<svg/>");
    let mut input = TransactionInput::new(target());
    input.preset = Some(ui);
    let plan = compile_transaction(&input).expect("a plan with a window and no content");
    let error = validate(state_root.path(), SelectedScope::User, &plan, None)
        .expect_err("a window with no content behind it");
    assert!(
        error.contains("neither installs it nor already owns it"),
        "{error}"
    );
}

#[test]
fn a_replacement_retires_the_content_the_window_it_replaces_owned() {
    let state_root = tempfile::tempdir().expect("a state root");
    let first = window(b"the first preset", b"<svg id='one'/>");
    let second = window(b"the second preset", b"<svg id='two'/>");
    let mut previous = install_plan(state_root.path(), SelectedScope::User, &first);
    let mut ledger = InstallLedger::new(app_id(), target(), SelectedScope::User);
    ledger.version = version();
    ledger.preset = Some(first.clone());
    for (path, digest) in content_files(state_root.path(), SelectedScope::User, &first) {
        ledger.resources.insert(
            ResourceKey::File {
                destination: zup_windows::plain_path_text(&path),
            },
            zup_exec::OwnedResource::File {
                destination: target_path(&path),
                sha256: digest,
                size: 1,
                source_relative: zup_core::RelativePath::new("content").expect("a relative path"),
                created_directories: Vec::new(),
                privilege: zup_core::Privilege::User,
            },
        );
    }

    let mut input = TransactionInput::new(target());
    for (path, sha256) in content_files(state_root.path(), SelectedScope::User, &second) {
        input.files.push(FileWork {
            key: ResourceKey::File {
                destination: zup_windows::plain_path_text(&path),
            },
            source_relative: zup_core::RelativePath::new("content").expect("a relative path"),
            destination: target_path(&path),
            precondition: FilePrecondition::Absent,
            expected_sha256: sha256,
            expected_size: 1,
            privilege: zup_core::Privilege::User,
            delta: FileDelta::Create,
            executable: false,
        });
    }
    for (path, sha256) in content_files(state_root.path(), SelectedScope::User, &first) {
        let key = ResourceKey::File {
            destination: zup_windows::plain_path_text(&path),
        };
        input.retired_keys.push(key.clone());
        input.removals.push(FileRemoval {
            key,
            kind: FileRemovalKind::RemoveOwned,
            scope: SelectedScope::User,
            privilege: zup_core::Privilege::User,
            destination: target_path(&path),
            sha256,
            size: 1,
            created_directories: Vec::new(),
        });
    }
    input.preset = Some(second.clone());
    let replacing = compile_transaction(&input).expect("the plan compiles");
    validate(
        state_root.path(),
        SelectedScope::User,
        &replacing,
        Some(ledger),
    )
    .expect("a generation that retires what it replaces");
    assert_eq!(
        replacing.preset,
        Some(second),
        "and it records the new window"
    );
    let _ = &mut previous;
}

#[test]
fn dropping_the_window_without_retiring_its_content_is_refused() {
    let state_root = tempfile::tempdir().expect("a state root");
    let ui = window(b"a preset executable", b"<svg/>");
    let mut ledger = InstallLedger::new(app_id(), target(), SelectedScope::User);
    ledger.version = version();
    ledger.preset = Some(ui.clone());
    for (path, digest) in content_files(state_root.path(), SelectedScope::User, &ui) {
        ledger.resources.insert(
            ResourceKey::File {
                destination: zup_windows::plain_path_text(&path),
            },
            zup_exec::OwnedResource::File {
                destination: target_path(&path),
                sha256: digest,
                size: 1,
                source_relative: zup_core::RelativePath::new("content").expect("a relative path"),
                created_directories: Vec::new(),
                privilege: zup_core::Privilege::User,
            },
        );
    }
    let empty = compile_transaction(&TransactionInput::new(target())).expect("an empty plan");
    let error = validate(
        state_root.path(),
        SelectedScope::User,
        &empty,
        Some(ledger.clone()),
    )
    .expect_err("a window's content cannot outlive the window");
    assert!(error.contains("stops presenting the window"), "{error}");

    validate(
        state_root.path(),
        SelectedScope::User,
        &retire_plan(state_root.path(), SelectedScope::User, &ui),
        Some(ledger),
    )
    .expect("and a plan that retires it is accepted");
}

#[test]
fn settings_survive_the_journal_unchanged() {
    let state_root = tempfile::tempdir().expect("a state root");
    let ui = window(b"a preset executable", b"<svg/>");
    let plan = install_plan(state_root.path(), SelectedScope::User, &ui);
    let journal = serde_json::to_vec(&plan).expect("the plan serializes");
    let read: TransactionPlan = serde_json::from_slice(&journal).expect("the plan parses");
    let recorded = read.preset.expect("the journal carries the window");
    assert_eq!(recorded.preset.settings, ui.preset.settings);
    assert_eq!(recorded, ui, "and the whole window, as one value");
}

#[test]
fn each_scope_keeps_its_own_window_under_its_own_authority() {
    let state_root = tempfile::tempdir().expect("a state root");
    let ui = window(b"a preset executable", b"<svg/>");
    let mut seen = Vec::new();
    for scope in [SelectedScope::User, SelectedScope::Machine] {
        let plan = install_plan(state_root.path(), scope, &ui);
        validate(state_root.path(), scope, &plan, None)
            .unwrap_or_else(|error| panic!("{scope}: {error}"));
        for file in &plan.nodes {
            let Some(meta) = file.meta.privilege else {
                continue;
            };
            assert_eq!(
                meta,
                scope.authorization(),
                "a {scope} window's content carries {scope}'s authority and no more"
            );
        }
        for (path, _) in content_files(state_root.path(), scope, &ui) {
            assert!(
                zup_bundle::is_content_path(
                    &zup_transaction::maintenance_root(state_root.path(), &app_id(), scope),
                    &zup_windows::plain_path_text(&path),
                ),
                "{} is recognised as {scope}'s own window content",
                path.display()
            );
            seen.push(path);
        }
    }
    assert_ne!(seen[0], seen[1], "two scopes are two storages");
}

#[test]
fn the_persisted_window_is_the_runtime_model() {
    let ui = window(b"a preset executable", b"<svg/>");
    let ledger: InstallLedger = serde_json::from_value(serde_json::json!({
        "schema": 2,
        "app_id": "com.zup.test-ui",
        "scope": "user",
        "target": "x86_64-pc-windows-msvc",
        "version": "1.0.0",
        "selected_components": [],
        "install_directory": null,
        "release": null,

        "preset": {
            "preset": {
                "name": "aurora",
                "version": "1.4.2",
                "protocol": zup_core_preset_protocol_version(),
                "required_capabilities": ["components"],
                "settings": { "hero": "Install Acme" },
            },
            "executable": ui.executable.to_hex(),
        },
        "committed_transaction": "00000000-0000-7000-8000-000000000000",
        "resources": [],
    }))
    .expect("a ledger carrying a window");
    let recorded = ledger.preset().expect("the window is present");
    assert_eq!(recorded.executable, ui.executable);
    assert_eq!(recorded.preset.name, ui.preset.name);
    assert_eq!(recorded.preset.version, ui.preset.version);
}

#[test]
fn a_ledger_of_a_superseded_format_is_refused() {
    let state_root = tempfile::tempdir().expect("a state root");
    let store = zup_windows::InstallLedgerStore::new(state_root.path());
    std::fs::create_dir_all(state_root.path().join("installations")).expect("the directory");
    std::fs::write(
        store.path_for(&app_id(), SelectedScope::User),
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "app_id": "com.zup.test-ui",
            "scope": "user",
            "target": "x86_64-pc-windows-msvc",
            "version": "1.0.0",
            "selected_components": [],
            "install_directory": null,
            "release": null,
            "committed_transaction": "00000000-0000-7000-8000-000000000000",
            "resources": [],
        }))
        .expect("the record serializes"),
    )
    .expect("the record is written");
    let read = store.load(&app_id(), SelectedScope::User);
    assert!(
        read.is_err(),
        "a record that says nothing about a window is refused rather than read as having none"
    );
}
