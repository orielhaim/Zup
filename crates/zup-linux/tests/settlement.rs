#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::path::Path;

use rstest::rstest;
use zup_core::SelectedScope;
use zup_linux::test_support::{
    MachineTestRoots, WorkerFault, machine_fixture, machine_v1_files, run_machine_faulted,
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
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let outcome = run_machine_faulted(&installer, &roots, Operation::Apply, fault);
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
            assert!(!ledger_published(&roots.state));
            LinuxLedgerStore::new(&roots.state)
                .repair_committed(&app_id(), SelectedScope::Machine)
                .expect("repair publishes the committed journal");
            assert!(ledger_published(&roots.state));
        }
        WorkerFault::None => unreachable!(),
    }
}
