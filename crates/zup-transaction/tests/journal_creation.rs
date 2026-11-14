#![cfg(feature = "test-faults")]
mod common;
mod crashes {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/journal_crashes.rs"
    ));
}
use zup_transaction::{FilesystemTransactionStore, TransactionRecord, TransactionStore};
fn open(path: &std::path::Path) -> FilesystemTransactionStore {
    FilesystemTransactionStore::private_at(path)
}
fn record() -> TransactionRecord {
    TransactionRecord::new(
        zup_core::TransactionId::new_v7(),
        common::sample_app_id(),
        zup_core::SelectedScope::User,
        common::sample_version(),
        common::sample_plan(),
    )
}
#[test]
fn creation_process() {
    crashes::child(open);
}
#[test]
fn interrupted_creation_does_not_publish_a_missing_journal() {
    crashes::matrix(open, record(), "creation_process", false);
}
#[test]
fn processes_serialize_creation_and_revision_cas() {
    crashes::races(open, record(), "creation_process", false);
}

#[rstest::rstest]
#[case("journal_namespace_created")]
#[case("journal_lock_published")]
#[case("journal_creation_reserved")]
fn interrupted_creation_does_not_block_unrelated_cleanup(#[case] point: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = open(&root);
    let publisher = common::Publisher::default();
    zup_transaction::TransactionCoordinator::new(store.clone())
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            common::sample_plan(),
        )
        .unwrap()
        .execute(&mut common::fake::FakeExecutor::new(), &publisher)
        .unwrap();
    let installed = publisher.0.lock().unwrap().clone().unwrap();
    let expected = zup_transaction::cleanup_candidates(&store, &installed, None).unwrap();
    let input = temp.path().join("input.json");
    std::fs::write(&input, serde_json::to_vec(&record()).unwrap()).unwrap();
    assert!(
        !std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "creation_process"])
            .env("ZUP_JOURNAL_CRASH_ROOT", &root)
            .env("ZUP_JOURNAL_CRASH_RECORD", input)
            .env("ZUP_TEST_FAULT_POINT", point)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        zup_transaction::cleanup_candidates(&open(&root), &installed, None).unwrap(),
        expected
    );
    for id in store.list_ids().unwrap() {
        store.load(&id).unwrap();
    }
}

#[rstest::rstest]
#[case::lock("transaction.lock")]
#[case::record("transaction.json")]
#[case::foreign_member("foreign.txt")]
fn reserved_creation_preserves_substituted_evidence(#[case] member: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = open(&root);
    let intact = record();
    store.create(&intact).unwrap();
    let input = temp.path().join("input.json");
    std::fs::write(&input, serde_json::to_vec(&record()).unwrap()).unwrap();
    assert!(
        !std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "creation_process"])
            .env("ZUP_JOURNAL_CRASH_ROOT", &root)
            .env("ZUP_JOURNAL_CRASH_RECORD", input)
            .env("ZUP_TEST_FAULT_POINT", "journal_record_published")
            .status()
            .unwrap()
            .success()
    );
    let evidence = root.join("journal-creation/pending").join(member);
    std::fs::write(&evidence, b"foreign evidence").unwrap();
    assert!(store.create(&record()).is_err());
    assert_eq!(std::fs::read(&evidence).unwrap(), b"foreign evidence");
    assert_eq!(store.list_ids().unwrap(), [intact.transaction_id]);
    assert_eq!(store.load(&intact.transaction_id).unwrap(), intact);
}

#[test]
fn published_missing_record_is_not_restored_from_a_creation_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = open(&root);
    let proposed = record();
    let input = temp.path().join("input.json");
    std::fs::write(&input, serde_json::to_vec(&proposed).unwrap()).unwrap();
    assert!(
        !std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "creation_process"])
            .env("ZUP_JOURNAL_CRASH_ROOT", &root)
            .env("ZUP_JOURNAL_CRASH_RECORD", input)
            .env("ZUP_TEST_FAULT_POINT", "journal_namespace_published")
            .status()
            .unwrap()
            .success()
    );
    let path = root
        .join("transactions")
        .join(proposed.transaction_id.to_string())
        .join("transaction.json");
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        store.create(&record()),
        Err(zup_transaction::StoreError::Corrupt(
            zup_transaction::CorruptReason::Missing
        ))
    ));
    assert!(store.list_ids().is_err());
    assert!(!path.exists());
    assert!(root.join("journal-creation/pending.json").exists());
}

#[test]
fn a_foreign_lock_inserted_during_initialization_is_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = open(&root);
    let input = temp.path().join("input.json");
    std::fs::write(&input, serde_json::to_vec(&record()).unwrap()).unwrap();
    let latch = temp.path().join("barrier");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "creation_process"])
        .env("ZUP_JOURNAL_CRASH_ROOT", &root)
        .env("ZUP_JOURNAL_CRASH_RECORD", input)
        .env("ZUP_TEST_FAULT_POINT", "journal_namespace_created")
        .env("ZUP_TEST_FAULT_ACTION", "pause")
        .env("ZUP_TEST_FAULT_LATCH", &latch)
        .spawn()
        .unwrap();
    let ready = latch.with_extension("ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let lock = root.join("journal-creation/pending/transaction.lock");
    std::fs::write(&lock, b"foreign evidence").unwrap();
    std::fs::write(latch.with_extension("release"), b"go").unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(std::fs::read(lock).unwrap(), b"foreign evidence");
    assert!(store.list_ids().unwrap().is_empty());
}
