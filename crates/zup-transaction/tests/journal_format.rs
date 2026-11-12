mod common;

use std::{io::Write, path::Path};
use zup_transaction::{
    FilesystemTransactionStore, NodeState, StoreError, TransactionChange as Change,
    TransactionRecord, TransactionStore,
};

fn create(root: &Path) -> (FilesystemTransactionStore, TransactionRecord) {
    let store = FilesystemTransactionStore::private_at(root);
    let record = TransactionRecord::new(
        zup_core::TransactionId::new_v7(),
        common::sample_app_id(),
        zup_core::SelectedScope::User,
        common::sample_version(),
        common::sample_plan(),
    );
    store.create(&record).unwrap();
    (store, record)
}

fn failure(i: usize) -> Change {
    Change::RecoveryFailure(Some(zup_transaction::TransactionFailure {
        operation: None,
        message: format!("retry {i}: {}", "evidence ".repeat(1000)),
    }))
}

#[rstest::rstest]
#[case::partial_header(&[1, 2, 3])]
#[case::partial_payload(&[20, 0, 0, 0, 0, 0, 0, 0, b'{'])]
#[case::complete_uncommitted_frame(&[2, 0, 0, 0, 0, 0, 0, 0, b'{', b'}'])]
fn only_bytes_past_the_committed_head_can_be_discarded(#[case] tail: &[u8]) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let id = record.transaction_id;
    let running = store
        .transition(
            &id,
            0,
            &[Change::Node(
                zup_plan::OperationId::new("ctrl:begin"),
                NodeState::Running,
            )],
        )
        .unwrap();
    let log = root
        .join("transactions")
        .join(id.to_string())
        .join("events-a.log");
    let committed_length = std::fs::metadata(&log).unwrap().len();
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    file.write_all(tail).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let restarted = FilesystemTransactionStore::private_at(&root);
    assert_eq!(restarted.load(&id).unwrap(), running);
    let next = restarted.transition(&id, running.revision, &[]).unwrap();
    assert_eq!(store.load(&id).unwrap(), next);
    assert!(std::fs::metadata(log).unwrap().len() > committed_length);
    assert_eq!(
        next.nodes[&zup_plan::OperationId::new("ctrl:begin")],
        NodeState::Running
    );
}

#[rstest::rstest]
#[case::checksum("events-a.log", false)]
#[case::truncated_committed_tail("events-a.log", true)]
#[case::missing_head("head.json", true)]
#[case::altered_intent("transaction.json", false)]
fn committed_damage_fails_closed_for_warm_and_cold_readers(
    #[case] member: &str,
    #[case] truncate: bool,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let id = record.transaction_id;
    store.transition(&id, 0, &[failure(0)]).unwrap();
    let path = root.join("transactions").join(id.to_string()).join(member);
    let mut bytes = std::fs::read(&path).unwrap();
    if truncate {
        bytes.truncate(bytes.len() / 2);
    } else {
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
    }
    std::fs::write(&path, &bytes).unwrap();
    assert!(store.load(&id).is_err());
    assert!(
        FilesystemTransactionStore::private_at(&root)
            .load(&id)
            .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn invalid_transitions_foreign_intent_and_stale_cas_do_not_publish() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let id = record.transaction_id;
    let node = zup_plan::OperationId::new("ctrl:begin");
    assert!(
        store
            .commit_changes(&id, 0, &[Change::Node(node.clone(), NodeState::Failed)])
            .is_err()
    );
    assert_eq!(store.load(&id).unwrap(), record);
    let next = store
        .transition(&id, 0, &[Change::Node(node, NodeState::Running)])
        .unwrap();
    assert!(matches!(
        store.commit_changes(&id, 0, &[]),
        Err(StoreError::RevisionConflict { .. })
    ));
    let mut foreign = next.clone();
    foreign.created_at = foreign.updated_at;
    foreign.touch();
    assert!(store.compare_and_swap(next.revision, &foreign).is_err());
    assert_eq!(store.load(&id).unwrap(), next);
}

#[rstest::rstest]
#[case::wrong_revision(false)]
#[case::replayed_revision(true)]
fn a_valid_checksum_cannot_authorize_discontinuous_history(#[case] replay: bool) {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let id = record.transaction_id;
    store.transition(&id, 0, &[failure(0)]).unwrap();
    let journal = root.join("transactions").join(id.to_string());
    let bytes = std::fs::read(journal.join("events-a.log")).unwrap();
    let mut event: serde_json::Value = serde_json::from_slice(&bytes[8..]).unwrap();
    let mut head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(journal.join("head.json")).unwrap()).unwrap();
    let previous: zup_core::Sha256Digest =
        serde_json::from_value(head[if replay { "tail" } else { "intent" }].clone()).unwrap();
    if !replay {
        event["revision"] = 3.into();
    }
    let event = serde_json::to_vec(&event).unwrap();
    let mut log = if replay { bytes } else { Vec::new() };
    log.extend_from_slice(&(event.len() as u64).to_le_bytes());
    log.extend_from_slice(&event);
    let mut hash = Sha256::new();
    hash.update(b"zup/journal/event/4\0");
    hash.update(previous.as_bytes());
    hash.update((event.len() as u64).to_le_bytes());
    hash.update(&event);
    head["tail"] = serde_json::to_value(zup_core::Sha256Digest::from_hasher(hash)).unwrap();
    head["length"] = (log.len() as u64).into();
    head["revision"] = if replay { 2 } else { 3 }.into();
    std::fs::write(journal.join("events-a.log"), &log).unwrap();
    std::fs::write(
        journal.join("head.json"),
        serde_json::to_vec(&head).unwrap(),
    )
    .unwrap();
    assert!(store.load(&id).is_err());
    assert!(
        FilesystemTransactionStore::private_at(&root)
            .load(&id)
            .is_err()
    );
}

#[test]
fn replacing_a_log_with_identical_bytes_does_not_preserve_its_authority() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let id = record.transaction_id;
    store.transition(&id, 0, &[failure(0)]).unwrap();
    let log = root
        .join("transactions")
        .join(id.to_string())
        .join("events-a.log");
    std::fs::rename(&log, log.with_extension("original")).unwrap();
    std::fs::copy(log.with_extension("original"), &log).unwrap();
    assert!(store.load(&id).is_err());
    assert!(
        FilesystemTransactionStore::private_at(&root)
            .load(&id)
            .is_err()
    );
}

#[test]
fn a_large_failed_install_preserves_compensation_evidence_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = FilesystemTransactionStore::private_at(&root);
    let mut input = common::effects();
    input.files = (0..128)
        .map(|i| common::file(&format!("payload-{i}.bin"), b"large failure fixture"))
        .collect();
    let settled = zup_transaction::TransactionCoordinator::new(store)
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            zup_plan::compile_deployment(&input).unwrap(),
        )
        .unwrap()
        .execute(
            &mut common::fake::FakeExecutor::new().fail_on_apply(200),
            &common::Publisher::default(),
        )
        .unwrap();
    assert_eq!(
        settled.outcome,
        zup_transaction::TransactionOutcome::RolledBack
    );
    let cold = FilesystemTransactionStore::private_at(&root);
    assert_eq!(
        cold.load(&settled.record.transaction_id).unwrap(),
        settled.record
    );
    assert!(
        settled
            .record
            .nodes
            .values()
            .filter(|state| matches!(state, NodeState::Compensated { .. }))
            .count()
            > 128
    );
}

#[test]
fn compaction_bounds_disk_and_preserves_all_verified_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, record) = create(&root);
    let (mut record, outcome) = zup_transaction::TransactionCoordinator::new(store.clone())
        .execute(
            record,
            &mut common::fake::FakeExecutor::new(),
            &common::Publisher::default(),
        )
        .unwrap();
    assert_eq!(outcome, zup_transaction::TransactionOutcome::Committed);
    let id = record.transaction_id;
    let journal = root.join("transactions").join(id.to_string());
    let mut sizes = Vec::new();
    for round in 0..3 {
        for i in 0..96 {
            record = store
                .transition(&id, record.revision, &[failure(round * 96 + i)])
                .unwrap();
        }
        let cold = FilesystemTransactionStore::private_at(&root);
        assert_eq!(cold.load(&id).unwrap(), record);
        sizes.push(
            std::fs::read_dir(&journal)
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum::<u64>(),
        );
    }
    assert!(journal.join("checkpoint-a.json").exists());
    assert!(journal.join("checkpoint-b.json").exists());
    assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 7);
    assert!(sizes.iter().all(|size| *size < 512 * 1024), "{sizes:?}");
    let head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(journal.join("head.json")).unwrap()).unwrap();
    let checkpoint = if head["slot"] == "b" {
        "checkpoint-b.json"
    } else {
        "checkpoint-a.json"
    };
    std::fs::remove_file(journal.join(checkpoint)).unwrap();
    assert!(store.load(&id).is_err());
    assert!(
        FilesystemTransactionStore::private_at(&root)
            .load(&id)
            .is_err()
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn journal_process() {
    let Some(root) = std::env::var_os("ZUP_FORMAT_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let id = std::env::var("ZUP_FORMAT_ID").unwrap().parse().unwrap();
    let store = FilesystemTransactionStore::private_at(root);
    let record = store.load(&id).unwrap();
    store
        .commit_changes(
            &id,
            record.revision,
            &[Change::Node(
                zup_plan::OperationId::new("ctrl:begin"),
                NodeState::Running,
            )],
        )
        .unwrap();
    std::fs::write(root.join("side-effect"), b"started").unwrap();
    for i in 0..48 {
        let record = store.load(&id).unwrap();
        store
            .commit_changes(&id, record.revision, &[failure(i)])
            .unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[rstest::rstest]
#[case("journal_frame_header_written")]
#[case("journal_frame_written")]
#[case("journal_frame_synced")]
#[case("journal_head_before")]
#[case("journal_head_published")]
#[case("journal_checkpoint_before")]
#[case("journal_checkpoint_written")]
#[case("journal_checkpoint_ready")]
#[case("journal_checkpoint_published")]
fn process_exit_at_every_commit_boundary_recovers_only_the_committed_prefix(#[case] point: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let (store, original) = create(&root);
    let id = original.transaction_id;
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "journal_process"])
        .env("ZUP_FORMAT_ROOT", &root)
        .env("ZUP_FORMAT_ID", id.to_string())
        .env("ZUP_TEST_FAULT_POINT", point)
        .status()
        .unwrap();
    assert!(!status.success(), "child did not reach {point}");
    let cold = FilesystemTransactionStore::private_at(&root);
    let recovered = cold.load(&id).unwrap();
    recovered.validate().unwrap();
    assert_eq!(recovered.plan, original.plan);
    if root.join("side-effect").exists() || point == "journal_head_published" {
        assert_eq!(
            recovered.nodes[&zup_plan::OperationId::new("ctrl:begin")],
            NodeState::Running
        );
    }
    if point == "journal_head_published" {
        assert_eq!(recovered.revision, 1);
    }
    if point.starts_with("journal_frame_") || point == "journal_head_before" {
        assert_eq!(recovered, original);
    }
    let next = cold
        .transition(&id, recovered.revision, &[failure(999)])
        .unwrap();
    assert_eq!(store.load(&id).unwrap(), next);
}
