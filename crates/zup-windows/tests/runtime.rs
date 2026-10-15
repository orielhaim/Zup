#![cfg(windows)]

use std::path::Path;

use tempfile::TempDir;
use zup_core::{
    App, AppId, NonEmptyString, Privilege, RelativePath, ResourceKey, SelectedScope, Target,
    hash_reader,
};
use zup_exec::LifecycleAction;
use zup_platform::{TargetFile, TargetPath, TargetPlan, TargetPlanSummary};
use zup_runtime::{ExecutionPolicy, InstallOutcome, RuntimeRequest, SessionError};
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, TransactionInput, compile_transaction,
};
use zup_windows::{InstallLedgerStore, PayloadOverlayIdentity, WindowsRuntimeBackend};

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn target_path(path: &Path) -> TargetPath {
    TargetPath::new(
        Target::parse("windows-x64").unwrap(),
        path.to_string_lossy(),
    )
    .unwrap()
}

fn target() -> Target {
    Target::parse("windows-x64").unwrap()
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

async fn run(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
    policy: ExecutionPolicy,
) -> Result<InstallOutcome, SessionError> {
    let control = zup_runtime::OperationControl::new(policy);
    zup_runtime::execute(backend, request, &control).await
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
    let outcome = run(&backend, request.clone(), ExecutionPolicy::Interactive)
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
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
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
    let error = run(&backend, request, ExecutionPolicy::NonInteractive)
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
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let ledger = InstallLedgerStore::new(&state_root)
        .load(&app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let request = RuntimeRequest {
        target: target(),
        app_id,
        app_version,
        scope: SelectedScope::User,
        transaction_plan: uninstall,
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
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
async fn a_stale_uninstall_is_rejected_after_the_generation_changes() {
    let root = TempDir::new().unwrap();
    let (request, backend) = request(&root, SelectedScope::User);
    let app_id = request.app_id.clone();
    let app_version = request.app_version.clone();
    let state_root = request.state_root.clone();
    let destination = root.path().join("install").join("app.exe");
    let payload_root = root.path().join("payload");
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let store = InstallLedgerStore::new(&state_root);
    let mut ledger = store.load(&app_id, SelectedScope::User).unwrap().unwrap();
    ledger.committed_transaction = "ffffffff-ffff-ffff-ffff-ffffffffffff".into();
    std::fs::write(
        store.path_for(&app_id, SelectedScope::User),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();
    let request = RuntimeRequest {
        target: target(),
        app_id,
        app_version,
        scope: SelectedScope::User,
        transaction_plan: uninstall,
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert!(
        matches!(outcome, InstallOutcome::Failed(ref detail) if detail.contains("installation generation")),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"installed");
    assert!(
        InstallLedgerStore::new(&state_root)
            .load(&ledger.app_id, SelectedScope::User)
            .unwrap()
            .is_some()
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
    let maintenance_target = TargetPath::new(target_triple, maintenance.to_string_lossy()).unwrap();
    let target_plan = TargetPlan {
        app: App {
            id: app_id.clone(),
            name: NonEmptyString::new("Apps Round Trip").unwrap(),
            version: version.clone(),
            publisher: None,
            main: None,
            description: None,
        },
        target: target_triple,
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
        target: target_triple,
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
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
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
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(
        zup_windows::inspect_uninstall_registration(SelectedScope::User, &app_id_text)
            .unwrap()
            .is_none()
    );
    assert!(!maintenance.exists());
}

#[tokio::test]
#[ignore = "requires elevated administrator; uses the authoritative machine root"]
async fn machine_runtime_uses_authoritative_work_for_modify_and_uninstall() {
    let root = TempDir::new().unwrap();
    let (mut install, backend) = request(&root, SelectedScope::Machine);
    install.app_id = AppId::new(format!(
        "com.zup.machine-probe-{}",
        uuid::Uuid::new_v4().simple()
    ))
    .unwrap();
    let machine = zup_windows::MachineRoot::acquire().expect("machine authority");
    let destination = root.path().join("install/app.exe");
    let ledgers = InstallLedgerStore::new(machine.path());
    for iteration in 0..2 {
        assert_eq!(
            run(&backend, install.clone(), ExecutionPolicy::Interactive)
                .await
                .unwrap(),
            InstallOutcome::Committed
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"installed");
        assert!(
            !install.state_root.exists(),
            "caller cannot choose machine state"
        );
        assert!(
            !install.work_root.exists(),
            "caller cannot choose machine work"
        );
        let mut input = file_input(&root, SelectedScope::Machine);
        input.observed_transaction = Some(
            ledgers
                .load(&install.app_id, SelectedScope::Machine)
                .unwrap()
                .unwrap()
                .committed_transaction,
        );
        let file = &mut input.files[0];
        file.precondition = FilePrecondition::Exact {
            size: 9,
            sha256: digest(b"installed"),
        };
        file.expected_sha256 = digest(b"modified");
        file.expected_size = 8;
        file.delta = FileDelta::Replace;
        std::fs::write(root.path().join("payload/app.exe"), b"modified").unwrap();
        let mut modify = install.clone();
        modify.app_version = "2.0.0".parse().unwrap();
        modify.transaction_plan = compile_transaction(&input).unwrap();
        let guard = if iteration == 0 {
            use std::os::windows::fs::OpenOptionsExt;
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(1)
                    .open(ledgers.path_for(&install.app_id, SelectedScope::Machine))
                    .unwrap(),
            )
        } else {
            None
        };
        let outcome = run(&backend, modify.clone(), ExecutionPolicy::Interactive)
            .await
            .unwrap();
        if let Some(guard) = guard {
            assert!(
                matches!(
                    outcome,
                    InstallOutcome::CommittedPendingReconciliation { .. }
                ),
                "{outcome:?}"
            );
            let ids = machine.transaction_store().list_ids().unwrap();
            let pending = ids
                .iter()
                .filter(|id| {
                    use zup_transaction::TransactionStore;
                    let record = machine.transaction_store().load(id).unwrap();
                    record.app_id == install.app_id && record.app_version == modify.app_version
                })
                .collect::<Vec<_>>();
            assert_eq!(pending.len(), 1);
            let work = machine.path().join("work").join(pending[0].to_string());
            assert!(
                work.is_dir(),
                "pending reconciliation retains rollback material"
            );
            drop(guard);
            ledgers
                .repair_committed(&install.app_id, SelectedScope::Machine)
                .unwrap();
            assert!(!work.exists(), "durable ledger publication releases work");
        } else {
            assert_eq!(outcome, InstallOutcome::Committed);
        }
        assert_eq!(std::fs::read(&destination).unwrap(), b"modified");
        assert_eq!(
            ledgers
                .load(&install.app_id, SelectedScope::Machine)
                .unwrap()
                .unwrap()
                .version,
            modify.app_version
        );
        let committed = ledgers
            .load(&install.app_id, SelectedScope::Machine)
            .unwrap()
            .unwrap();
        assert!(
            !machine
                .path()
                .join("work")
                .join(&committed.committed_transaction)
                .exists()
        );
        let mut uninstall = modify;
        uninstall.transaction_plan = zup_windows::plan_target_lifecycle(
            LifecycleAction::Uninstall,
            &install.app_id,
            SelectedScope::Machine,
            None,
            machine.path(),
        )
        .unwrap();
        assert_eq!(
            run(&backend, uninstall, ExecutionPolicy::Interactive)
                .await
                .unwrap(),
            InstallOutcome::Committed
        );
        assert!(!destination.exists());
        assert!(
            ledgers
                .load(&install.app_id, SelectedScope::Machine)
                .unwrap()
                .is_none()
        );
        assert!(!install.work_root.exists());
        std::fs::write(root.path().join("payload/app.exe"), b"installed").unwrap();
    }
}
