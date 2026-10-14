#![cfg(all(target_os = "linux", feature = "test-support"))]

use std::path::Path;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use zup_core::{AppId, SelectedScope, Target};
use zup_exec::InstallLedger;
use zup_linux::LinuxRuntimeBackend;
use zup_linux::test_support::{compose_fixture, v1_files};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, Operation, OperationControl, RuntimeControl, RuntimeRequest,
};
use zup_transaction::{
    FilesystemTransactionStore, InstallationLock, TransactionCoordinator, TransactionInput,
    TransactionStore, compile_transaction,
};

fn app_id() -> AppId {
    AppId::new("com.example.fresh").expect("id")
}

fn plan() -> zup_transaction::TransactionPlan {
    compile_transaction(&TransactionInput::new(
        Target::parse("linux-x64").expect("target"),
    ))
    .expect("plan")
}

fn request(root: &Path, plan: zup_transaction::TransactionPlan) -> RuntimeRequest {
    RuntimeRequest {
        target: plan.target,
        app_id: app_id(),
        app_version: semver::Version::new(1, 0, 0),
        scope: SelectedScope::User,
        transaction_plan: plan,
        state_root: root.to_path_buf(),
        work_root: root.join("work"),
        recovery_id: None,
        bootstrap: None,
        release: None,
    }
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

fn backend(installer: &Path) -> LinuxRuntimeBackend {
    LinuxRuntimeBackend {
        installer: installer.to_path_buf(),
        operation: Operation::Install,
        selection: zup_runtime::OperationSelection {
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        install_dir_override: None,
        generated: Default::default(),
    }
}

fn installer(scratch: &Path) -> std::path::PathBuf {
    compose_fixture(scratch, "setup", "1.0.0", &v1_files())
}

#[test]
fn a_held_lock_blocks_repair_and_a_later_plan_is_checked_again() {
    let scratch = tempfile::tempdir().expect("scratch");
    let installer = installer(scratch.path());
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let pending = plan();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::new(&state))
        .begin(
            app_id(),
            SelectedScope::User,
            semver::Version::new(1, 0, 0),
            pending.clone(),
        )
        .expect("journal");
    let journal = FilesystemTransactionStore::new(&state)
        .load(&open.record().transaction_id)
        .expect("journal reads");
    drop(open);

    let key = InstallationLock::lock_key(app_id().as_str(), "user");
    let held = InstallationLock::try_acquire(&state, &key)
        .expect("lock")
        .expect("acquired");
    let barrier = Arc::new(Barrier::new(2));
    let (sender, receiver) = mpsc::channel();
    let wait = Arc::clone(&barrier);
    let state_for_wait = state.clone();
    let installer_for_wait = installer.clone();
    let pending_for_wait = pending.clone();
    thread::spawn(move || {
        wait.wait();
        let (_operation, runtime) = controls();
        let report = backend(&installer_for_wait)
            .execute_blocking(&request(&state_for_wait, pending_for_wait), &runtime);
        let _ = sender.send(report);
    });
    barrier.wait();
    let report = receiver.recv().expect("the waiter finished");
    let report = report.expect("busy is a result");
    assert!(
        matches!(report.outcome, InstallOutcome::Busy { .. }),
        "contention is busy, not a stale execution: {:?}",
        report.outcome
    );
    let still = FilesystemTransactionStore::new(&state)
        .load(&journal.transaction_id)
        .expect("the journal was not repaired while the lock was held");
    assert_eq!(still.phase, journal.phase);
    drop(held);

    let (_operation, runtime) = controls();
    let report = backend(&installer)
        .execute_blocking(&request(&state, pending), &runtime)
        .expect("repair reports recovery");
    assert!(
        matches!(report.outcome, InstallOutcome::RecoveryRequired),
        "the pending journal is reconciled only after the lock: {:?}",
        report.outcome
    );
}

#[test]
fn an_operation_prepared_earlier_is_rejected_when_the_ledger_changes() {
    let scratch = tempfile::tempdir().expect("scratch");
    let installer = installer(scratch.path());
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let pending = plan();
    let mut ledger = InstallLedger::new(app_id(), pending.target, SelectedScope::User);
    ledger.version = semver::Version::new(1, 0, 0);
    ledger.committed_transaction = "00000000-0000-0000-0000-000000000001".into();
    let store = zup_linux::LinuxLedgerStore::new(&state);
    store_ledger(&store.path_for(&app_id(), SelectedScope::User), &ledger);
    store
        .validate_plan(&app_id(), SelectedScope::User, &ledger.version, &pending)
        .expect("the plan matches the ledger it was prepared against");

    let key = InstallationLock::lock_key(app_id().as_str(), "user");
    let held = InstallationLock::try_acquire(&state, &key)
        .expect("lock")
        .expect("acquired");
    ledger.target = Target::parse("linux-aarch64").expect("other target");
    store_ledger(&store.path_for(&app_id(), SelectedScope::User), &ledger);
    drop(held);

    let (_operation, runtime) = controls();
    let error = match backend(&installer).execute_blocking(&request(&state, pending), &runtime) {
        Err(error) => error,
        Ok(report) => panic!(
            "a stale plan was executed: {}",
            report_name(&report.outcome)
        ),
    };
    assert!(error.to_string().contains("target identity"), "{error}");
    assert!(
        !state.join("transactions").exists(),
        "rejection happens before a journal is created"
    );
}

#[test]
fn cancellation_before_the_transaction_leaves_no_journal() {
    let scratch = tempfile::tempdir().expect("scratch");
    let installer = installer(scratch.path());
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let (operation, runtime) = controls();
    operation.cancellation().cancel();
    let report = backend(&installer)
        .execute_blocking(&request(&state, plan()), &runtime)
        .expect("cancellation is a result");
    assert!(matches!(report.outcome, InstallOutcome::Cancelled));
    assert!(!state.join("transactions").exists());
}

fn report_name(outcome: &InstallOutcome) -> String {
    format!("{outcome:?}")
}

fn store_ledger(path: &Path, ledger: &InstallLedger) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("ledger directory");
    }
    std::fs::write(path, serde_json::to_vec_pretty(ledger).expect("ledger")).expect("ledger");
}
