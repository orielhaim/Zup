#![cfg(target_os = "linux")]

#[path = "common/faults.rs"]
mod faults;

use std::path::Path;

use faults::{Boundary, InterruptedExecutor, InterruptedStore};
use zup_bundle::DirectoryPayloadSource;
use zup_core::{AppId, Privilege, RelativePath, ResourceKey, SelectedScope, Target, hash_reader};
use zup_linux::{LinuxFileExecutor, LinuxLedgerStore};
use zup_platform::TargetPath;
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, FilesystemTransactionStore, TransactionCoordinator,
    TransactionInput, TransactionOutcome, TransactionStore, compile_transaction, recover,
};

const PAYLOAD: &[u8] = b"verified application content";
const BOUNDARIES: &[Boundary] = &[
    Boundary::BeforeMutation,
    Boundary::AfterMutation,
    Boundary::BeforeJournal,
    Boundary::AfterJournal,
    Boundary::BeforeCommit,
    Boundary::AfterCommit,
    Boundary::DuringRollback,
    Boundary::BeforeLedger,
    Boundary::AfterLedger,
];

fn executor(root: &Path, record: &zup_transaction::TransactionRecord) -> LinuxFileExecutor {
    let mut executor =
        LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(root.join("payload")));
    executor.register_plan(&record.plan).unwrap();
    executor.set_transaction_id(record.transaction_id);
    executor
}

#[test]
#[ignore = "launched by process_interruption_recovers_real_files"]
fn interrupted_child() {
    let root = std::path::PathBuf::from(std::env::var_os("ZUP_INTERRUPTION_ROOT").unwrap());
    let index: usize = std::env::var("ZUP_INTERRUPTION_POINT")
        .unwrap()
        .parse()
        .unwrap();
    let boundary = BOUNDARIES[index];
    let target = Target::host().unwrap();
    let destination =
        TargetPath::new(target, root.join("installed/tool").to_string_lossy()).unwrap();
    let mut input = TransactionInput::new(target);
    input.files.push(FileWork {
        key: ResourceKey::File {
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new("tool").unwrap(),
        destination,
        precondition: FilePrecondition::Absent,
        expected_sha256: hash_reader(PAYLOAD).unwrap().1,
        expected_size: PAYLOAD.len() as u64,
        privilege: Privilege::User,
        delta: FileDelta::Create,
        executable: false,
    });
    let store = InterruptedStore {
        inner: FilesystemTransactionStore::private_at(root.join("state")),
        boundary,
    };
    let open = TransactionCoordinator::new(store)
        .begin(
            AppId::new("com.example.interrupted").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            compile_transaction(&input).unwrap(),
        )
        .unwrap();
    let mut executor = InterruptedExecutor {
        inner: executor(&root, &open),
        boundary,
    };
    let settled = open.execute(&mut executor).unwrap();
    boundary.reach(Boundary::BeforeLedger);
    LinuxLedgerStore::new(root.join("state"))
        .publish_committed(&settled.record, SelectedScope::User)
        .unwrap();
    boundary.reach(Boundary::AfterLedger);
    panic!("interruption point was not reached");
}

#[test]
fn process_interruption_recovers_real_files() {
    for index in 0..BOUNDARIES.len() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("payload")).unwrap();
        std::fs::create_dir(root.path().join("installed")).unwrap();
        std::fs::write(root.path().join("payload/tool"), PAYLOAD).unwrap();
        std::fs::write(root.path().join("installed/user-notes"), b"user content").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "interrupted_child", "--ignored", "--nocapture"])
            .env("ZUP_INTERRUPTION_ROOT", root.path())
            .env("ZUP_INTERRUPTION_POINT", index.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(77),
            "point {index}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let store = FilesystemTransactionStore::private_at(root.path().join("state"));
        let ids = store.list_ids().unwrap();
        assert_eq!(ids.len(), 1);
        let mut previous = None;
        for _ in 0..2 {
            let record = store.load(&ids[0]).unwrap();
            let mut executor = executor(root.path(), &record);
            let (record, outcome) = recover(record, &store, &mut executor).unwrap();
            record.validate().unwrap();
            if let Some(previous) = previous {
                assert_eq!(
                    outcome, previous,
                    "repeated recovery changed the outcome at {index}"
                );
            }
            previous = Some(outcome);
            let installed = root.path().join("installed/tool");
            match outcome {
                TransactionOutcome::Committed => {
                    LinuxLedgerStore::new(root.path().join("state"))
                        .publish_committed(&record, SelectedScope::User)
                        .unwrap();
                    assert_eq!(std::fs::read(&installed).unwrap(), PAYLOAD);
                }
                TransactionOutcome::RolledBack => assert!(!installed.exists()),
                TransactionOutcome::RecoveryRequired => {
                    if installed.exists() {
                        assert_eq!(std::fs::read(&installed).unwrap(), PAYLOAD);
                    }
                }
            }
            assert_eq!(
                std::fs::read(root.path().join("installed/user-notes")).unwrap(),
                b"user content"
            );
        }
    }
}
