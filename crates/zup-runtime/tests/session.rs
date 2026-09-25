//! Runtime session tests (local user-scope path).

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zup_core::{
    ProtocolScheme, RelativePath, ResourceKey, SelectedScope, Sha256Digest, hash_reader,
};
use zup_exec::{
    ExecutionPlan, ExecutionSummary, FileOperation, FileOperationKind, FilePrecondition,
    ObservedProtocolState, ProtocolOperation, ProtocolOperationKind,
};
use zup_platform::{CommandSpec, TargetPath};
use zup_runtime::{
    CancellationHandle, ExecutionPolicy, InstallOutcome, OverlayPolicy, RuntimeEvent,
    RuntimeRequest, SessionError, discover_recovery, run_install_control,
    run_install_control_with_policy, run_local_install,
};

fn digest(b: &[u8]) -> Sha256Digest {
    hash_reader(b).unwrap().1
}

fn tpath(s: &str) -> TargetPath {
    TargetPath::new(PathBuf::from(s)).unwrap()
}

fn sample_request(scope: SelectedScope) -> RuntimeRequest {
    let dir = TempDir::new().unwrap().keep();
    let destination = dir.join("install").join("a.exe");
    let dest = destination.to_string_lossy().into_owned();
    let source_relative = RelativePath::new("tools/a.exe").unwrap();
    let payload_root = dir.join("payload");
    std::fs::create_dir_all(payload_root.join("tools")).unwrap();
    std::fs::write(payload_root.join(source_relative.as_str()), b"hello").unwrap();
    let execution = ExecutionPlan {
        selected_components: vec![],
        install_directory: None,
        uninstall: false,
        removals: vec![],
        files: vec![FileOperation {
            key: ResourceKey::File {
                destination: dest.clone(),
            },
            kind: FileOperationKind::Create,
            destination: tpath(&dest),
            source_relative: source_relative.clone(),
            precondition: FilePrecondition::Absent,
            expected_sha256: digest(b"hello"),
            expected_size: 5,
            conflict: None,
        }],
        shortcuts: vec![],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_types: vec![],
        uninstall_entries: vec![],
        summary: ExecutionSummary {
            files_create: 1,
            requires_elevation: scope == SelectedScope::Machine,
            ..Default::default()
        },
    };
    RuntimeRequest {
        app_id: zup_core::AppId::new("com.acme.acme").unwrap(),
        app_version: "1.0.0".parse().unwrap(),
        scope,
        execution_plan: execution,
        state_root: dir.join("state"),
        work_root: dir.join("work"),
        payload_root,
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        recovery_id: None,
        bootstrap: None,
    }
}

#[cfg(windows)]
#[tokio::test]
async fn noninteractive_machine_scope_requires_an_already_elevated_process() {
    if zup_windows::is_process_elevated().unwrap() {
        return;
    }
    let request = sample_request(SelectedScope::Machine);
    let (events, _) = tokio::sync::broadcast::channel(16);
    let error = run_install_control_with_policy(
        request,
        CancellationHandle::new(),
        events,
        ExecutionPolicy::NonInteractive,
        OverlayPolicy::Cleanup,
    )
    .await
    .expect_err("unelevated noninteractive machine install must fail");
    assert!(matches!(error, SessionError::ElevationRequired));
}

#[tokio::test]
async fn local_user_scope_runs() {
    let request = sample_request(SelectedScope::User);
    let destination = request.execution_plan.files[0]
        .destination
        .as_path()
        .to_path_buf();
    let result = run_local_install(request).await;
    let (outcome, _session) = match result {
        Ok(pair) => pair,
        Err(e) => panic!("local install failed: {e}"),
    };
    assert_eq!(outcome, InstallOutcome::Committed);
    assert_eq!(std::fs::read(destination).unwrap(), b"hello");
}

#[tokio::test]
async fn local_generated_overlay_is_used_and_cleaned_after_commit() {
    let mut request = sample_request(SelectedScope::User);
    request.execution_plan.files[0].source_relative =
        RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let identity = zup_windows::PayloadOverlayIdentity::from_execution_plan(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.execution_plan,
    )
    .unwrap();
    let overlay = identity.path_under(&request.state_root).unwrap();
    let overlay_file = overlay.join(request.execution_plan.files[0].source_relative.as_str());
    std::fs::create_dir_all(overlay_file.parent().unwrap()).unwrap();
    std::fs::write(&overlay_file, b"hello").unwrap();
    request.payload_overlay_root = Some(overlay.clone());
    request.payload_overlay_base_root = Some(request.state_root.clone());
    let destination = request.execution_plan.files[0]
        .destination
        .as_path()
        .to_path_buf();

    let (outcome, _) = run_local_install(request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert_eq!(std::fs::read(destination).unwrap(), b"hello");
    assert!(!overlay.exists());
}

#[tokio::test]
async fn local_generated_overlay_is_cleaned_after_request_failure() {
    let mut request = sample_request(SelectedScope::User);
    request.execution_plan.files[0].source_relative =
        RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let identity = zup_windows::PayloadOverlayIdentity::from_execution_plan(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.execution_plan,
    )
    .unwrap();
    let overlay = identity.path_under(&request.state_root).unwrap();
    let overlay_file = overlay.join(request.execution_plan.files[0].source_relative.as_str());
    std::fs::create_dir_all(overlay_file.parent().unwrap()).unwrap();
    std::fs::write(&overlay_file, b"hello").unwrap();
    request.payload_overlay_root = Some(overlay.clone());
    request.payload_overlay_base_root = Some(request.state_root.clone());
    request.execution_plan.files[0].conflict = Some(zup_exec::Conflict::TargetNonFile {
        path: "blocked".into(),
    });

    let error = run_local_install(request).await.unwrap_err();
    assert!(matches!(error, SessionError::PlanInvalid(_)));
    assert!(!overlay.exists());
}

#[tokio::test]
async fn local_generated_overlay_is_cleaned_after_execution_failure() {
    let mut request = sample_request(SelectedScope::User);
    request.execution_plan.files[0].source_relative =
        RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let identity = zup_windows::PayloadOverlayIdentity::from_execution_plan(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.execution_plan,
    )
    .unwrap();
    let overlay = identity.path_under(&request.state_root).unwrap();
    let overlay_file = overlay.join(request.execution_plan.files[0].source_relative.as_str());
    std::fs::create_dir_all(overlay_file.parent().unwrap()).unwrap();
    std::fs::write(&overlay_file, b"hello").unwrap();
    request.payload_overlay_root = Some(overlay.clone());
    request.payload_overlay_base_root = Some(request.state_root.clone());
    let destination = request.execution_plan.files[0]
        .destination
        .as_path()
        .to_path_buf();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"old").unwrap();
    let file = &mut request.execution_plan.files[0];
    file.kind = FileOperationKind::Replace;
    file.precondition = FilePrecondition::Exact {
        size: 3,
        sha256: digest(b"old"),
    };

    let (outcome, _) = run_local_install(request).await.unwrap();
    assert!(matches!(outcome, InstallOutcome::Failed(message) if message.contains("ownership")));
    assert!(!overlay.exists());
}

#[tokio::test]
async fn recovery_rejects_mismatched_generated_overlay_before_mutation() {
    use zup_transaction::{
        FilesystemTransactionStore, TransactionId, TransactionRecord, TransactionStore,
        compile_transaction,
    };

    let mut request = sample_request(SelectedScope::User);
    request.execution_plan.files[0].source_relative =
        RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let destination = request.execution_plan.files[0]
        .destination
        .as_path()
        .to_path_buf();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"untouched").unwrap();
    let plan = compile_transaction(&request.execution_plan).unwrap();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        request.app_id.clone(),
        request.scope,
        request.app_version.clone(),
        plan,
    );
    let store = FilesystemTransactionStore::new(&request.state_root);
    store.create(&record).unwrap();
    let identity = zup_windows::PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &record.plan,
    )
    .unwrap();
    let overlay = identity.path_under(&request.state_root).unwrap();
    let overlay_file = overlay.join("__zup_plugins__/generated.exe");
    std::fs::create_dir_all(overlay_file.parent().unwrap()).unwrap();
    std::fs::write(&overlay_file, b"HELLO").unwrap();
    request.execution_plan = ExecutionPlan::default();
    request.payload_overlay_root = Some(overlay.clone());
    request.payload_overlay_base_root = Some(request.state_root.clone());
    request.recovery_id = Some(record.transaction_id);

    let error = run_local_install(request).await.unwrap_err();
    assert!(matches!(error, SessionError::PlanInvalid(_)));
    assert_eq!(std::fs::read(destination).unwrap(), b"untouched");
    assert!(overlay.exists());
    assert_eq!(
        store.load(&record.transaction_id).unwrap().phase,
        zup_transaction::TransactionPhase::Prepared
    );
}

#[tokio::test]
async fn control_channel_reports_cumulative_progress_for_the_complete_plan() {
    let request = sample_request(SelectedScope::User);
    let (events, _) = tokio::sync::broadcast::channel(64);
    let mut rx = events.subscribe();
    let outcome = run_install_control(request, CancellationHandle::new(), events)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);

    let progress = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            RuntimeEvent::Progress {
                completed, total, ..
            } => Some((completed, total)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(progress.len() >= 2);
    assert!(progress.iter().all(|(_, total)| *total > 0));
    assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert_eq!(progress.last().unwrap().0, progress.last().unwrap().1);
}

#[tokio::test]
async fn local_replace_refuses_unowned_file() {
    let mut request = sample_request(SelectedScope::User);
    let file = &mut request.execution_plan.files[0];
    let destination = file.destination.as_path().to_path_buf();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"old").unwrap();
    file.kind = FileOperationKind::Replace;
    file.precondition = FilePrecondition::Exact {
        size: 3,
        sha256: digest(b"old"),
    };

    let (outcome, _) = run_local_install(request).await.unwrap();

    assert!(matches!(outcome, InstallOutcome::Failed(message) if message.contains("ownership")));
    assert_eq!(std::fs::read(destination).unwrap(), b"old");
}

#[tokio::test]
async fn registry_drift_rolls_back_earlier_file_and_keeps_ledger_absent() {
    let mut request = sample_request(SelectedScope::User);
    let scheme = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
    let destination = request.execution_plan.files[0]
        .destination
        .as_path()
        .to_path_buf();
    let command = CommandSpec::new(tpath(&destination.to_string_lossy()), vec!["%1".into()]);
    request.execution_plan.protocols.push(ProtocolOperation {
        key: ResourceKey::Protocol {
            scheme: ProtocolScheme::new(&scheme).unwrap(),
        },
        kind: ProtocolOperationKind::Create,
        scheme: ProtocolScheme::new(&scheme).unwrap(),
        command,
        previous: ObservedProtocolState::Absent,
        scope: SelectedScope::User,
        conflict: None,
    });
    let classes = windows_registry::CURRENT_USER
        .create("Software\\Classes")
        .unwrap();
    let foreign = classes.create(&scheme).unwrap();
    foreign.set_string("URL Protocol", "").unwrap();
    foreign
        .create("shell\\open\\command")
        .unwrap()
        .set_string("", r"C:\foreign.exe %1")
        .unwrap();
    let state_root = request.state_root.clone();
    let app_id = request.app_id.clone();
    let (outcome, _) = run_local_install(request).await.unwrap();
    assert_eq!(outcome, InstallOutcome::RolledBack);
    assert!(!destination.exists());
    assert!(
        zup_windows::InstallLedgerStore::new(state_root)
            .load(&app_id, SelectedScope::User)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        foreign
            .open("shell\\open\\command")
            .unwrap()
            .get_string("")
            .unwrap(),
        r"C:\foreign.exe %1"
    );
    classes.remove_tree(&scheme).unwrap();
}

#[tokio::test]
async fn invalid_plan_rejected_before_run() {
    let mut request = sample_request(SelectedScope::User);
    // Force a conflict so compilation fails.
    request.execution_plan.files[0].conflict =
        Some(zup_exec::Conflict::TargetNonFile { path: "x".into() });
    let err = run_local_install(request).await.unwrap_err();
    assert!(matches!(err, SessionError::PlanInvalid(_)));
}

#[tokio::test]
async fn invalid_plan_rejected_before_bootstrap_mutation() {
    let mut request = sample_request(SelectedScope::User);
    request.execution_plan.files[0].conflict =
        Some(zup_exec::Conflict::TargetNonFile { path: "x".into() });
    let plan = zup_bootstrap::BootstrapPlan::new(
        zup_bootstrap::BootstrapKey {
            app_id: request.app_id.clone(),
            app_version: request.app_version.clone(),
            scope: request.scope,
        },
        Vec::new(),
    )
    .unwrap();
    let bootstrap = zup_bootstrap::BoundBootstrapPlan::new(plan, BTreeMap::new()).unwrap();
    let bootstrap_state_root = request.state_root.join("bootstrap-state");
    let quarantine_root = request.state_root.join("quarantine");
    request.bootstrap = Some(zup_runtime::BootstrapRequest {
        plan: bootstrap,
        state_root: bootstrap_state_root.clone(),
        quarantine_root: quarantine_root.clone(),
    });

    let (events, _) = tokio::sync::broadcast::channel(8);
    let err = run_install_control(request, CancellationHandle::new(), events)
        .await
        .unwrap_err();

    assert!(matches!(err, SessionError::PlanInvalid(_)));
    assert!(!bootstrap_state_root.exists());
    assert!(!quarantine_root.exists());
}

#[test]
fn discovery_empty_state_root() {
    let dir = TempDir::new().unwrap();
    let found = discover_recovery(&dir.path().join("state"));
    assert!(found.is_empty());
}
