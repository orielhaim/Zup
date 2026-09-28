//! Durable store tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use tempfile::TempDir;
use zup_core::AppId;
use zup_transaction::{
    CorruptReason, FilesystemTransactionStore, NodeState, OperationId, StoreError, TransactionId,
    TransactionRecord, TransactionStore, compile_transaction,
};

mod common;
use common::{sample_input, sample_plan};

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

/// The plan's mutating nodes, which a caller may move to `Running` in any order.
fn mutating_nodes(record: &TransactionRecord) -> Vec<OperationId> {
    record
        .plan
        .execution_order
        .iter()
        .filter(|id| !id.as_str().starts_with("ctrl:"))
        .cloned()
        .collect()
}

/// The plan's first barrier, which `Prepared` accepts in any state a `Running`
/// or `Failed` mark can leave behind.
fn first_barrier(record: &TransactionRecord) -> OperationId {
    record
        .plan
        .execution_order
        .iter()
        .find(|id| id.as_str().starts_with("ctrl:"))
        .cloned()
        .expect("plan has a barrier")
}

/// Hold every swap until both contenders have read the record they are about to
/// replace.
///
/// Two updaters that read one revision and then swap it is the race the update
/// path exists to survive. Rostering the readers makes that certain instead of
/// leaving it to the scheduler, so a passing test proves the retry rather than
/// hoping it was needed.
struct RendezvousStore {
    inner: FilesystemTransactionStore,
    contenders: usize,
    loads: Mutex<usize>,
    arrived: Condvar,
    swaps: AtomicUsize,
    conflicts: AtomicUsize,
}

impl RendezvousStore {
    fn new(root: &std::path::Path, contenders: usize) -> Self {
        Self {
            inner: FilesystemTransactionStore::new(root),
            contenders,
            loads: Mutex::new(0),
            arrived: Condvar::new(),
            swaps: AtomicUsize::new(0),
            conflicts: AtomicUsize::new(0),
        }
    }
}

impl TransactionStore for RendezvousStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        self.inner.create(record)
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        let record = self.inner.load(id)?;
        *self.loads.lock().unwrap() += 1;
        self.arrived.notify_all();
        Ok(record)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError> {
        if self.swaps.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut loads = self.loads.lock().unwrap();
            while *loads < self.contenders {
                loads = self.arrived.wait(loads).unwrap();
            }
        }
        let result = self.inner.compare_and_swap(expected_revision, updated);
        if matches!(result, Err(StoreError::RevisionConflict { .. })) {
            self.conflicts.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

/// Let a competing write win the first `budget` races.
///
/// The competing write is a real one, so the caller's in-flight record really is
/// stale when it tries to swap. The budget is finite on purpose: a store that
/// stole every race forever would livelock any caller, and proving the retry
/// converges needs an adversary that eventually lets the writer through.
struct RevisionRaceStore {
    inner: FilesystemTransactionStore,
    interloper: OperationId,
    budget: AtomicUsize,
    conflicts: AtomicUsize,
}

impl RevisionRaceStore {
    fn new(root: &std::path::Path, interloper: OperationId, budget: usize) -> Self {
        Self {
            inner: FilesystemTransactionStore::new(root),
            interloper,
            budget: AtomicUsize::new(budget),
            conflicts: AtomicUsize::new(0),
        }
    }
}

impl TransactionStore for RevisionRaceStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        self.inner.create(record)
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        self.inner.load(id)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError> {
        if self
            .budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_err()
        {
            return self.inner.compare_and_swap(expected_revision, updated);
        }
        let id = updated.transaction_id;
        let interloper = self.interloper.clone();
        self.inner.update(&id, &mut |fresh| {
            fresh.nodes.insert(interloper.clone(), NodeState::Running);
            Ok(())
        })?;
        self.conflicts.fetch_add(1, Ordering::SeqCst);
        Err(StoreError::RevisionConflict {
            expected: expected_revision,
        })
    }
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

/// Two updaters that read the same revision must both land, not race to a
/// single winner with the loser told it lost.
#[test]
fn concurrent_updaters_on_one_record_both_land() {
    let (dir, store) = store();
    let rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    let contenders: Vec<OperationId> = mutating_nodes(&rec).into_iter().take(2).collect();
    assert_eq!(contenders.len(), 2, "sample plan needs two mutating nodes");

    let store = std::sync::Arc::new(RendezvousStore::new(dir.path(), 2));
    let updaters: Vec<_> = contenders
        .iter()
        .cloned()
        .map(|op| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                store.update(&id, &mut |fresh| {
                    let current =
                        fresh
                            .nodes
                            .get(&op)
                            .cloned()
                            .ok_or_else(|| StoreError::Rejected {
                                id: id.to_string(),
                                reason: format!("missing node {op}"),
                            })?;
                    let next = current.transition(NodeState::Running).map_err(|error| {
                        StoreError::Rejected {
                            id: id.to_string(),
                            reason: error.to_string(),
                        }
                    })?;
                    fresh.nodes.insert(op.clone(), next);
                    Ok(())
                })
            })
        })
        .collect();

    let mut landed: Vec<u64> = updaters
        .into_iter()
        .map(|updater| {
            updater
                .join()
                .unwrap()
                .expect("both updaters land")
                .revision
        })
        .collect();
    landed.sort_unstable();

    assert!(
        store.conflicts.load(Ordering::SeqCst) >= 1,
        "the two updaters must actually have raced"
    );
    let final_record = FilesystemTransactionStore::new(dir.path())
        .load(&id)
        .unwrap();
    assert_eq!(
        final_record.revision, 2,
        "one revision per committed update, not one per attempt"
    );
    assert_eq!(
        landed,
        vec![1, 2],
        "each updater commits its own revision, in some order"
    );
    for op in &contenders {
        assert!(
            matches!(final_record.nodes.get(op), Some(NodeState::Running)),
            "both mutations survive: {op}"
        );
    }
    final_record.validate().expect("final state is consistent");
}

/// An update whose swap is lost to a competing write must re-read and re-apply,
/// keeping both writes.
#[test]
fn update_retries_a_lost_swap_instead_of_failing() {
    let (dir, store) = store();
    let rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    let target = mutating_nodes(&rec).remove(0);
    let budget = 2;
    let racer = RevisionRaceStore::new(dir.path(), first_barrier(&rec), budget);

    let committed = racer
        .update(&id, &mut |fresh| {
            let current =
                fresh
                    .nodes
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| StoreError::Rejected {
                        id: id.to_string(),
                        reason: "missing node".into(),
                    })?;
            let next =
                current
                    .transition(NodeState::Running)
                    .map_err(|error| StoreError::Rejected {
                        id: id.to_string(),
                        reason: error.to_string(),
                    })?;
            fresh.nodes.insert(target.clone(), next);
            Ok(())
        })
        .expect("a lost swap is not fatal");

    assert_eq!(
        racer.conflicts.load(Ordering::SeqCst),
        budget,
        "the interloper must have won every race it budgeted"
    );
    assert_eq!(
        committed.revision,
        budget as u64 + 1,
        "each stolen revision is re-read over, and the retry commits on top"
    );
    let final_record = store.load(&id).unwrap();
    assert_eq!(final_record, committed);
    assert!(matches!(
        final_record.nodes.get(&target),
        Some(NodeState::Running)
    ));
    assert!(matches!(
        final_record.nodes.get(&first_barrier(&rec)),
        Some(NodeState::Running)
    ));
    final_record.validate().expect("final state is consistent");
}

#[test]
fn corrupted_json_rejected() {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let path = record_path(&dir, &rec.transaction_id);
    std::fs::write(path, b"not json").unwrap();
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
}

#[test]
fn unsupported_schema_rejected() {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    let path = record_path(&dir, &rec.transaction_id);
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
    let path = record_path(&dir, &rec.transaction_id);
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["plan_hash"] = serde_json::json!("00".repeat(32));
    std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
}

fn record_path(dir: &TempDir, id: &TransactionId) -> std::path::PathBuf {
    dir.path()
        .join("transactions")
        .join(id.to_string())
        .join("transaction.json")
}

#[test]
fn duplicate_create_leaves_the_committed_record_alone() {
    let (_dir, store) = store();
    let original = record();
    store.create(&original).unwrap();
    let mut advanced = original.clone();
    advanced.touch();
    store.compare_and_swap(0, &advanced).unwrap();

    let err = store.create(&original).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists { .. }), "{err:?}");
    assert_eq!(store.load(&original.transaction_id).unwrap(), advanced);
}

#[test]
fn cas_with_a_non_successor_revision_is_refused_without_writing() {
    let (_dir, store) = store();
    let mut rec = record();
    store.create(&rec).unwrap();
    rec.revision = 5;
    let err = store.compare_and_swap(0, &rec).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
    assert_eq!(store.load(&rec.transaction_id).unwrap().revision, 0);
}

/// Every replacement leaves exactly one complete JSON document and no temp
/// siblings for a later reader to trip over.
#[test]
fn replacement_leaves_complete_json_and_no_temp_files() {
    let (dir, store) = store();
    let mut rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    for expected in 0..3 {
        rec.touch();
        store.compare_and_swap(expected, &rec).unwrap();
        let on_disk: TransactionRecord =
            serde_json::from_slice(&std::fs::read(record_path(&dir, &id)).unwrap()).unwrap();
        assert_eq!(on_disk, rec);
    }
    let mut entries: Vec<String> = std::fs::read_dir(record_path(&dir, &id).parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["transaction.json", "transaction.lock"]);
}

/// A fresh store — what a recovery pass after a crash opens — reads the last
/// committed revision, not the first.
#[test]
fn reopened_store_reads_the_latest_committed_record() {
    let (dir, store) = store();
    let mut rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    let barrier = first_barrier(&rec);
    rec.nodes.insert(barrier.clone(), NodeState::Running);
    rec.touch();
    store.compare_and_swap(0, &rec).unwrap();
    drop(store);

    let loaded = FilesystemTransactionStore::new(dir.path())
        .load(&id)
        .unwrap();
    assert_eq!(loaded, rec);
    assert!(matches!(
        loaded.nodes.get(&barrier),
        Some(NodeState::Running)
    ));
}

#[test]
fn load_of_an_unknown_transaction_is_missing() {
    let (_dir, store) = store();
    let err = store.load(&TransactionId::new_v7()).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(CorruptReason::Missing)),
        "{err:?}"
    );
}

#[test]
fn compile_is_deterministic() {
    let exec = sample_input();
    let a = compile_transaction(&exec).unwrap();
    let b = compile_transaction(&exec).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.fingerprint(), b.fingerprint());
}
