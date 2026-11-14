#![cfg(all(target_os = "linux", feature = "test-support"))]

use std::path::Path;
use zup_core::{SelectedScope, hash_bytes};
use zup_linux::test_support::{IsolatedUser, compose_fixture, run_installer, v1_files, v2_files};
use zup_linux::{LinuxInstallationStore, LinuxOutcome, LinuxRuntimeBackend, LinuxSessionHost};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, Operation, OperationControl, OperationInput,
    OperationSelection, RuntimeControl, RuntimeRequest,
};
use zup_session::SessionHost;
use zup_transaction::{
    FilesystemTransactionStore, ResourceLock, TransactionCoordinator, TransactionStore,
};

fn prepare(installer: &Path, state: &Path) -> (LinuxRuntimeBackend, RuntimeRequest) {
    let mut host = LinuxSessionHost::open(installer.to_owned());
    let build = host.describe(installer).unwrap();
    let context = host
        .observe(&build.installer, SelectedScope::User, Some(state))
        .unwrap();
    let bound = host.bind(&context).unwrap();
    let prepared = zup_runtime::prepare_operation(
        &OperationInput {
            build: &build,
            installer,
            scope: SelectedScope::User,
            operation: Operation::Upgrade,
            selection: OperationSelection::default(),
            prior: context.ledger.as_ref(),
            state_root: state,
            cancellation: &zup_plan::NeverCancelled,
        },
        &bound,
    )
    .unwrap();
    let backend = LinuxRuntimeBackend {
        installer: installer.to_owned(),
        operation: Operation::Upgrade,
        selection: prepared.selection.clone(),
        generated: prepared
            .generated_files()
            .iter()
            .map(|file| (file.source_relative.to_string(), file.bytes.clone()))
            .collect(),
    };
    let request = zup_runtime::runtime_request(
        &prepared,
        state.to_owned(),
        zup_runtime::RequestOptions::default(),
    );
    (backend, request)
}

fn controls() -> (OperationControl, RuntimeControl) {
    let operation = OperationControl::new(ExecutionPolicy::NonInteractive);
    let runtime = RuntimeControl::new(
        operation.cancellation(),
        operation.events(),
        operation.policy(),
    );
    (operation, runtime)
}

#[test]
fn stale_deployment_selection_is_refused_before_effects() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().unwrap();
    let initial = compose_fixture(scratch.path(), "initial", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&initial, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let updated = compose_fixture(scratch.path(), "updated", "1.1.0", &v2_files());
    let (backend, request) = prepare(&updated, &user.state);
    assert!(matches!(
        run_installer(&updated, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let payload = user.programs().join("tool/tool");
    let before = hash_bytes(&std::fs::read(&payload).unwrap());
    let store = FilesystemTransactionStore::private_at(&user.state);
    let journals = store.list_ids().unwrap();
    let (_operation, control) = controls();
    assert!(backend.execute_blocking(&request, &control).is_err());
    assert_eq!(hash_bytes(&std::fs::read(payload).unwrap()), before);
    assert_eq!(store.list_ids().unwrap(), journals);
}

#[test]
fn a_held_lock_prevents_reconciling_pending_work() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().unwrap();
    let initial = compose_fixture(scratch.path(), "initial", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&initial, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let updated = compose_fixture(scratch.path(), "updated", "1.1.0", &v2_files());
    let (backend, request) = prepare(&updated, &user.state);
    let store = FilesystemTransactionStore::private_at(&user.state);
    let open = TransactionCoordinator::new(store.clone())
        .begin(
            request.app_id.clone(),
            request.scope,
            request.app_version.clone(),
            request.operation.deployment().unwrap().plan.clone(),
        )
        .unwrap();
    let pending = open.record().clone();
    drop(open);
    let key = ResourceLock::lock_key(request.app_id.as_str(), "user");
    let held = ResourceLock::try_acquire(&user.state, &key)
        .unwrap()
        .unwrap();
    let (_operation, control) = controls();
    let report = backend.execute_blocking(&request, &control).unwrap();
    assert!(matches!(report.outcome, InstallOutcome::Busy { .. }));
    assert_eq!(store.load(&pending.transaction_id).unwrap(), pending);
    drop(held);
    let report = backend.execute_blocking(&request, &control).unwrap();
    assert!(matches!(
        report.outcome,
        InstallOutcome::RecoveryRequired { .. }
    ));
    assert_eq!(store.load(&pending.transaction_id).unwrap(), pending);
}

#[test]
fn a_rechecksummed_record_cannot_replace_authenticated_installation_state() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().unwrap();
    let initial = compose_fixture(scratch.path(), "initial", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&initial, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let app = zup_core::AppId::new("com.example.tool").unwrap();
    let store = LinuxInstallationStore::new(&user.state);
    let mut record = store.load(&app, SelectedScope::User).unwrap().unwrap();
    record.current.version = "99.0.0".parse().unwrap();
    record.current.requirements.app.version = record.current.version.clone();
    let path = store.path_for(&app, SelectedScope::User);
    let forged = record.encode().unwrap();
    zup_storage::publish_private(&path, &forged).unwrap();
    assert!(store.load(&app, SelectedScope::User).is_err());
    assert!(store.repair_committed(&app, SelectedScope::User).is_err());
    assert_eq!(std::fs::read(path).unwrap(), forged);
}
