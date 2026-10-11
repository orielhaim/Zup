#![cfg(windows)]

use std::path::Path;

use tempfile::TempDir;
use zup_core::{
    App, AppId, NonEmptyString, Privilege, RelativePath, ResourceKey, SelectedScope, TargetTriple,
    hash_reader,
};
use zup_exec::{LifecycleAction, OwnedResource};
use zup_platform::{TargetFile, TargetPath, TargetPlan, TargetPlanSummary};
use zup_runtime::{ExecutionPolicy, InstallOutcome, RuntimeRequest, SessionError};
use zup_transaction::{
    FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork, TransactionInput,
    compile_transaction,
};
use zup_windows::{InstallLedgerStore, PayloadOverlayIdentity, WindowsRuntimeBackend};

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn target_path(path: &Path) -> TargetPath {
    TargetPath::new(
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        path.to_string_lossy(),
    )
    .unwrap()
}

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn file_input(root: &TempDir, scope: SelectedScope) -> TransactionInput {
    let destination = root.path().join("install").join("app.exe");
    let source = RelativePath::new("app.exe").unwrap();
    let mut input = TransactionInput::new(target());
    input.files.push(FileWork {
        key: ResourceKey::File {
            destination: destination.to_string_lossy().into_owned(),
        },
        source_relative: source,
        destination: target_path(&destination),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(b"installed"),
        expected_size: 9,
        privilege: if scope == SelectedScope::Machine {
            Privilege::System
        } else {
            Privilege::User
        },
        delta: FileDelta::Create,
        executable: false,
    });
    input
}

fn request(root: &TempDir, scope: SelectedScope) -> (RuntimeRequest, WindowsRuntimeBackend) {
    let state_root = root.path().join("state");
    let payload_root = root.path().join("payload");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::write(payload_root.join("app.exe"), b"installed").unwrap();
    let plan = compile_transaction(&file_input(root, scope)).unwrap();
    let request = RuntimeRequest {
        target: target(),
        app_id: AppId::new("com.zup.windows-runtime").unwrap(),
        app_version: "1.0.0".parse().unwrap(),
        scope,
        transaction_plan: plan,
        state_root,
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    (request, backend)
}

#[tokio::test]
async fn local_backend_commits_files_and_publishes_ledger() {
    let root = TempDir::new().unwrap();
    let (request, backend) = request(&root, SelectedScope::User);
    let destination = root.path().join("install").join("app.exe");
    let (outcome, _) = zup_windows::run_install(&backend, request.clone())
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert_eq!(std::fs::read(destination).unwrap(), b"installed");
    let ledger = InstallLedgerStore::new(&request.state_root)
        .load(&request.app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.version.to_string(), "1.0.0");
}

#[tokio::test]
async fn generated_overlay_is_consumed_and_cleaned_by_adapter() {
    let root = TempDir::new().unwrap();
    let (mut request, _) = request(&root, SelectedScope::User);
    let source = RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let file = &mut request
        .transaction_plan
        .nodes
        .iter_mut()
        .find(|node| matches!(node.kind, zup_transaction::NodeKind::FileMutation { .. }))
        .unwrap()
        .meta;
    file.source_relative = Some(source.clone());
    let identity = PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.transaction_plan,
    )
    .unwrap();
    let base = zup_windows::payload_overlay_base_root(&request.state_root, request.scope).unwrap();
    let overlay = identity.path_under(&base).unwrap();
    std::fs::create_dir_all(overlay.join("__zup_plugins__")).unwrap();
    std::fs::write(overlay.join("__zup_plugins__/generated.exe"), b"installed").unwrap();
    let payload_root = root.path().join("payload");
    let backend = WindowsRuntimeBackend::from_path(payload_root, Some(overlay.clone())).unwrap();
    let (outcome, _) = zup_windows::run_install(&backend, request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(!overlay.exists());
}

#[tokio::test]
async fn noninteractive_machine_scope_reports_authorization_required() {
    if zup_windows::is_process_elevated().unwrap() {
        return;
    }
    let root = TempDir::new().unwrap();
    let (request, backend) = request(&root, SelectedScope::Machine);
    let (events, _) = tokio::sync::broadcast::channel(32);
    let error = zup_windows::run_install_control_with_policy(
        &backend,
        request,
        zup_runtime::CancellationHandle::new(),
        events,
        ExecutionPolicy::NonInteractive,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SessionError::AuthorizationRequired));
}

#[tokio::test]
async fn backend_lifecycle_uninstall_removes_owned_files_and_ledger() {
    let root = TempDir::new().unwrap();
    let (request, backend) = request(&root, SelectedScope::User);
    let app_id = request.app_id.clone();
    let app_version = request.app_version.clone();
    let state_root = request.state_root.clone();
    let destination = root.path().join("install").join("app.exe");
    let payload_root = root.path().join("payload");
    let (outcome, _) = zup_windows::run_install(&backend, request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let ledger = InstallLedgerStore::new(&state_root)
        .load(&app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    let mut input = TransactionInput::new(target());
    input.uninstall = true;
    for (key, owned) in &ledger.resources {
        input.retired_keys.push(key.clone());
        let OwnedResource::File {
            destination,
            sha256,
            size,
            created_directories,
            ..
        } = owned
        else {
            continue;
        };
        input.removals.push(FileRemoval {
            key: key.clone(),
            kind: FileRemovalKind::RemoveOwned,
            scope: SelectedScope::User,
            privilege: Privilege::User,
            destination: destination.clone(),
            sha256: *sha256,
            size: *size,
            created_directories: created_directories.clone(),
        });
    }
    let request = RuntimeRequest {
        target: target(),
        app_id,
        app_version,
        scope: SelectedScope::User,
        transaction_plan: compile_transaction(&input).unwrap(),
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    let (outcome, _) = zup_windows::run_install(&backend, request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(!destination.exists());
    assert!(
        InstallLedgerStore::new(&state_root)
            .load(&ledger.app_id, SelectedScope::User)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn apps_features_registration_is_opaque_and_round_trips() {
    let root = TempDir::new().unwrap();
    let state_root = root.path().join("state");
    let payload_root = root.path().join("payload");
    let install_directory = root.path().join("install");
    let maintenance = install_directory.join("maintenance.exe");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::create_dir_all(&install_directory).unwrap();
    std::fs::write(payload_root.join("maintenance.bin"), b"maintenance").unwrap();
    let app_id = AppId::new(format!(
        "com.zup.apps-roundtrip-{}",
        uuid::Uuid::now_v7().simple()
    ))
    .unwrap();
    let app_id_text = app_id.as_str().to_owned();
    let version = semver::Version::parse("1.0.0").unwrap();
    let target_triple = target();
    let maintenance_target = TargetPath::new(
        target_triple.clone(),
        maintenance.to_string_lossy().as_ref(),
    )
    .unwrap();
    let target_plan = TargetPlan {
        app: App {
            id: app_id.clone(),
            name: NonEmptyString::new("Apps Round Trip").unwrap(),
            version: version.clone(),
            publisher: None,
            main: None,
            description: None,
        },
        target: target_triple.clone(),
        scope: SelectedScope::User,
        install_directory: target_path(&install_directory),
        selected_components: Vec::new(),
        prerequisites: Vec::new(),
        files: vec![TargetFile {
            key: ResourceKey::Maintenance {
                app_id: app_id.as_str().to_owned(),
                version: version.to_string(),
                destination: maintenance_target.to_string(),
            },
            source_relative: RelativePath::new("maintenance.bin").unwrap(),
            destination: maintenance_target,
            size: 11,
            sha256: digest(b"maintenance"),
            privilege: Privilege::User,

            executable: true,
        }],
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
        summary: TargetPlanSummary {
            file_count: 1,
            install_bytes: 11,
            resource_count: 1,
            requires_authorization: false,
            selected_component_count: 0,
            prerequisite_count: 0,
            download_bytes: 11,
        },
        preset: None,
    };
    let plan = zup_windows::plan_target_lifecycle(
        LifecycleAction::Install,
        &app_id,
        SelectedScope::User,
        Some(&target_plan),
        &state_root,
    )
    .unwrap();
    assert!(plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            zup_transaction::NodeKind::BackendOperation { .. }
        )
    }));
    let backend =
        zup_windows::WindowsRuntimeBackend::from_path(payload_root.clone(), None).unwrap();
    let request = RuntimeRequest {
        target: target_triple.clone(),
        app_id: app_id.clone(),
        app_version: version.clone(),
        scope: SelectedScope::User,
        transaction_plan: plan,
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let (outcome, _) = zup_windows::run_install(&backend, request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let registration =
        zup_windows::inspect_uninstall_registration(SelectedScope::User, app_id.as_str())
            .unwrap()
            .unwrap();
    assert!(matches!(
        registration.values.get("DisplayName"),
        Some(zup_windows::AppsFeaturesValue::String(value)) if value == "Apps Round Trip"
    ));
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let request = RuntimeRequest {
        target: target_triple,
        app_id,
        app_version: version,
        scope: SelectedScope::User,
        transaction_plan: uninstall,
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let (outcome, _) = zup_windows::run_install(&backend, request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(
        zup_windows::inspect_uninstall_registration(SelectedScope::User, &app_id_text)
            .unwrap()
            .is_none()
    );
    assert!(!maintenance.exists());
}
