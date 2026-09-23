//! Runtime session tests (local user-scope path).

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
    InstallOutcome, RuntimeRequest, SessionError, discover_recovery, run_local_install,
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
        external_actions: vec![],
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
        recovery_id: None,
    }
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

#[test]
fn discovery_empty_state_root() {
    let dir = TempDir::new().unwrap();
    let found = discover_recovery(&dir.path().join("state"));
    assert!(found.is_empty());
}
