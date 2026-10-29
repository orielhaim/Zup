#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::path::Path;

use rstest::rstest;
use zup_core::SelectedScope;
use zup_linux::test_support::{
    MachineTestRoots, WorkerFault, machine_fixture, machine_v1_files, machine_v2_files,
    run_machine_faulted,
};
use zup_linux::{LinuxLedgerStore, LinuxOutcome};
use zup_runtime::Operation;
use zup_transaction::{FilesystemTransactionStore, TransactionPhase, TransactionStore};

fn app_id() -> zup_core::AppId {
    zup_core::AppId::new("com.example.tool").expect("an id")
}

fn journals(state: &Path) -> Vec<zup_transaction::TransactionRecord> {
    let store = FilesystemTransactionStore::private_at(state);
    let directory = state.join("transactions");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse().ok()?;
            store
                .load(&zup_transaction::TransactionId::from_uuid(id))
                .ok()
        })
        .collect()
}

fn ledger_published(state: &Path) -> bool {
    LinuxLedgerStore::new(state)
        .load(&app_id(), SelectedScope::Machine)
        .expect("the ledger reads")
        .is_some()
}

#[rstest]
#[case::ipc_before_begin(WorkerFault::IpcBeforeBegin)]
#[case::ipc_after_begin(WorkerFault::IpcAfterBegin)]
#[case::cancel_after_begin(WorkerFault::CancelAfterBegin)]
#[case::progress_during_execute(WorkerFault::ProgressDuringExecute)]
#[case::disconnect_after_commit(WorkerFault::DisconnectAfterCommit)]
#[case::ledger_publish(WorkerFault::FailLedgerPublish)]
fn machine_execution_settles_without_the_client(#[case] fault: WorkerFault) {
    let base = tempfile::tempdir().expect("an isolated base");
    let roots = MachineTestRoots::isolate_in(base.path());
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let mut installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let action = if fault == WorkerFault::FailLedgerPublish {
        assert!(matches!(
            run_machine_faulted(&installer, &roots, Operation::Apply, WorkerFault::None),
            Ok(LinuxOutcome::Committed { .. })
        ));
        installer = machine_fixture(scratch.path(), "v2", "1.1.0", &machine_v2_files());
        Operation::Upgrade
    } else {
        Operation::Apply
    };
    let outcome = run_machine_faulted(&installer, &roots, action, fault);
    let records = journals(&roots.state);
    match fault {
        WorkerFault::IpcBeforeBegin => {
            assert!(
                outcome.is_err(),
                "a disconnect before begin fails closed: {outcome:?}"
            );
            assert!(records.is_empty(), "no journal exists before begin");
            assert!(!ledger_published(&roots.state));
        }
        WorkerFault::IpcAfterBegin | WorkerFault::CancelAfterBegin => {
            assert!(
                outcome.is_err(),
                "cancellation before execution is not a commit: {outcome:?}"
            );
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].phase, TransactionPhase::RolledBack);
            assert!(!ledger_published(&roots.state));
        }
        WorkerFault::ProgressDuringExecute | WorkerFault::DisconnectAfterCommit => {
            assert!(
                matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
                "telemetry loss does not undo a commit: {outcome:?}"
            );
            assert!(
                records
                    .iter()
                    .any(|record| record.phase == TransactionPhase::Committed)
            );
            assert!(ledger_published(&roots.state));
        }
        WorkerFault::FailLedgerPublish => {
            assert!(
                matches!(
                    outcome,
                    Ok(LinuxOutcome::CommittedPendingReconciliation { .. })
                ),
                "an unpublished commit is not a rollback: {outcome:?}"
            );
            assert!(
                records
                    .iter()
                    .any(|record| record.phase == TransactionPhase::Committed)
            );
            let Ok(LinuxOutcome::CommittedPendingReconciliation {
                transaction,
                detail,
            }) = &outcome
            else {
                unreachable!()
            };
            assert!(
                detail.contains(&transaction.to_string())
                    && detail.contains(app_id().as_str())
                    && detail.contains(&roots.state.display().to_string())
            );
            assert_eq!(
                LinuxLedgerStore::new(&roots.state)
                    .load(&app_id(), SelectedScope::Machine)
                    .unwrap()
                    .unwrap()
                    .version
                    .to_string(),
                "1.0.0"
            );
            let record = records
                .iter()
                .find(|record| record.transaction_id.as_uuid() == *transaction)
                .unwrap();
            let id = record.transaction_id;
            let backup = record
                .plan
                .nodes
                .iter()
                .find_map(|node| match record.receipt(&node.id) {
                    Some(zup_transaction::OperationReceipt::ReplaceFile {
                        backup_path, ..
                    }) => Some(std::path::PathBuf::from(backup_path)),
                    _ => None,
                })
                .unwrap();
            let original = std::fs::read(&backup).unwrap();
            let recover = || {
                let session = zup_protocol::SessionId::new_v7();
                let (mut client, mut worker) = std::os::unix::net::UnixStream::pair().unwrap();
                let worker_roots = roots.roots.clone();
                let worker_exe = installer.clone();
                let intent = zup_protocol::PrepareOperation {
                    recovery_id: Some(id.as_uuid()),
                    operation: "recover".into(),
                    force_files: false,
                    install_dir_override: None,
                    selected_components: Vec::new(),
                    disabled_components: Vec::new(),
                    expected_plan_digest: None,
                    app_id: app_id().to_string(),
                    app_version: "1.1.0".into(),
                    scope: "machine".into(),
                    target: records[0].target,
                    state_root: roots.state.display().to_string(),
                };
                let process = std::process::id();
                let uid = rustix::process::geteuid().as_raw();
                let thread = std::thread::spawn(move || {
                    zup_linux::test_support::serve_worker_isolated(
                        &mut worker,
                        &worker_roots,
                        uid,
                        process,
                        session,
                        &worker_exe,
                    )
                });
                let recovered = zup_linux::test_support::drive_client_isolated(
                    &mut client,
                    session,
                    &intent,
                    "",
                    &intent.target,
                );
                drop(client);
                thread.join().unwrap().unwrap();
                recovered.unwrap()
            };
            std::fs::write(&backup, b"unowned content").unwrap();
            for _ in 0..2 {
                let pending = recover();
                let LinuxOutcome::CommittedPendingReconciliation { detail: latest, .. } = pending
                else {
                    panic!("{pending:?}")
                };
                assert!(latest.contains(detail));
                assert_eq!(std::fs::read(&backup).unwrap(), b"unowned content");
            }
            std::fs::write(&backup, original).unwrap();
            for _ in 0..2 {
                assert!(matches!(recover(), LinuxOutcome::Committed { .. }));
            }
            assert!(ledger_published(&roots.state));
            assert_eq!(
                FilesystemTransactionStore::private_at(&roots.state)
                    .load(&id)
                    .unwrap()
                    .reconciliation,
                zup_transaction::ReconciliationState::Complete
            );
        }
        WorkerFault::None => unreachable!(),
    }
}
