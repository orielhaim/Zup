//! Durable store tests.

use tempfile::TempDir;
use zup_core::AppId;
use zup_transaction::{
    FilesystemTransactionStore, StoreError, TransactionId, TransactionRecord, TransactionStore,
    compile_transaction,
};

mod common;
use common::{sample_execution, sample_plan};

fn store() -> (TempDir, FilesystemTransactionStore) {
    let dir = TempDir::new().unwrap();
    let s = FilesystemTransactionStore::new(dir.path());
    (dir, s)
}

fn record() -> TransactionRecord {
    let plan = sample_plan();
    TransactionRecord::new(
        TransactionId::new_v7(),
        AppId::new("com.acme.acme").unwrap(),
        zup_core::SelectedScope::User,
        "1.4.0".parse().unwrap(),
        plan,
    )
}

#[test]
fn create_load_roundtrip() {
    let (_dir, store) = store();
    let rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded, rec);
}

#[test]
fn second_create_rejected() {
    let (_dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let err = store.create(&rec).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists { .. }));
}

#[test]
fn update_increments_revision() {
    let (_dir, store) = store();
    let mut rec = record();
    store.create(&rec).unwrap();
    rec.touch();
    store.compare_and_swap(0, &rec).unwrap();
    let loaded = store.load(&rec.transaction_id).unwrap();
    assert_eq!(loaded.revision, 1);
}

#[test]
fn stale_cas_rejected() {
    let (_dir, store) = store();
    let mut rec = record();
    store.create(&rec).unwrap();
    // First coordinator updates rev 0 → 1.
    rec.touch();
    store.compare_and_swap(0, &rec).unwrap();
    // Second coordinator still holds rev 0 and tries to write rev 1.
    let mut stale = record();
    stale.transaction_id = rec.transaction_id;
    stale.touch();
    let err = store.compare_and_swap(0, &stale).unwrap_err();
    assert!(
        matches!(err, StoreError::RevisionConflict { .. }),
        "{err:?}"
    );
}

#[test]
fn corrupted_json_rejected() {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let path = dir
        .path()
        .join("transactions")
        .join(rec.transaction_id.to_string())
        .join("transaction.json");
    std::fs::write(path, b"not json").unwrap();
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
}

#[test]
fn unsupported_schema_rejected() {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let path = dir
        .path()
        .join("transactions")
        .join(rec.transaction_id.to_string())
        .join("transaction.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["schema"] = serde_json::json!(99);
    std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
}

#[test]
fn plan_hash_mismatch_rejected() {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let path = dir
        .path()
        .join("transactions")
        .join(rec.transaction_id.to_string())
        .join("transaction.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["plan_hash"] = serde_json::json!("00".repeat(32));
    std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
}

#[test]
fn survives_reopening_store() {
    let (dir, _store) = store();
    let rec = record();
    let id = rec.transaction_id;
    {
        let store = FilesystemTransactionStore::new(dir.path());
        store.create(&rec).unwrap();
    }
    let store = FilesystemTransactionStore::new(dir.path());
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.transaction_id, id);
}

#[test]
fn compile_is_deterministic() {
    let exec = sample_execution();
    let a = compile_transaction(&exec).unwrap();
    let b = compile_transaction(&exec).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.fingerprint(), b.fingerprint());
}
