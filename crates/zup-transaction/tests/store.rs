//! Durable store tests.

use rstest::rstest;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use tempfile::TempDir;
use zup_core::AppId;
use zup_core::TransactionId;
use zup_plan::OperationId;
use zup_transaction::{
    CorruptReason, FilesystemTransactionStore, NodeState, StoreError, TransactionRecord,
    TransactionStore,
};

mod common;
use common::sample_plan;

fn store() -> (TempDir, FilesystemTransactionStore) {
    let dir = TempDir::new().unwrap();
    let s = FilesystemTransactionStore::private_at(dir.path().join("state"));
    (dir, s)
}

#[rstest]
#[case::unknown_identity(false)]
#[case::missing_journal(true)]
fn transaction_discovery_refuses_unknown_entries(#[case] valid_identity: bool) {
    let (directory, store) = store();
    let existing = record();
    store.create(&existing).unwrap();
    let foreign = if valid_identity {
        TransactionId::new_v7().to_string()
    } else {
        "invalid-identity".into()
    };
    let path = directory.path().join("state/transactions").join(foreign);
    std::fs::create_dir(&path).unwrap();
    let observed = store.list_ids().and_then(|ids| {
        ids.iter()
            .map(|id| store.load(id))
            .collect::<Result<Vec<_>, _>>()
    });
    assert!(observed.is_err());
    assert!(path.exists());
    assert_eq!(store.load(&existing.transaction_id).unwrap(), existing);
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
            inner: FilesystemTransactionStore::private_at(root.join("state")),
            contenders,
            loads: Mutex::new(0),
            arrived: Condvar::new(),
            swaps: AtomicUsize::new(0),
            conflicts: AtomicUsize::new(0),
        }
    }
}

impl TransactionStore for RendezvousStore {
    fn reserve_installation(
        &self,
        record: &TransactionRecord,
    ) -> Result<zup_transaction::InstallationReservation, zup_transaction::StoreError> {
        self.inner.reserve_installation(record)
    }
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
            inner: FilesystemTransactionStore::private_at(root.join("state")),
            interloper,
            budget: AtomicUsize::new(budget),
            conflicts: AtomicUsize::new(0),
        }
    }
}

impl TransactionStore for RevisionRaceStore {
    fn reserve_installation(
        &self,
        record: &TransactionRecord,
    ) -> Result<zup_transaction::InstallationReservation, zup_transaction::StoreError> {
        self.inner.reserve_installation(record)
    }
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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
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

/// A second create must not clobber the revision already committed under that
/// id: the loser of a duplicate is told it lost, and the journal is untouched.
#[test]
fn second_create_rejected() {
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
    let mut rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    common::reserve(&store, &mut rec);
    let base_revision = rec.revision;
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
    let final_record = FilesystemTransactionStore::private_at(dir.path().join("state"))
        .load(&id)
        .unwrap();
    assert_eq!(
        final_record.revision,
        base_revision + 2,
        "one revision per committed update, not one per attempt"
    );
    assert_eq!(
        landed,
        vec![base_revision + 1, base_revision + 2],
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
    let mut rec = record();
    let id = rec.transaction_id;
    store.create(&rec).unwrap();
    common::reserve(&store, &mut rec);
    let base_revision = rec.revision;
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
        base_revision + budget as u64 + 1,
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

/// Anything the store cannot vouch for - unreadable bytes, an unknown schema, a
/// plan that no longer hashes to the plan it is carrying - is refused rather
/// than recovered, because a half-trusted record replays as if it were whole.
#[rstest]
#[case::malformed_json(write_not_json)]
#[case::unknown_schema(write_unknown_schema)]
#[case::foreign_plan_hash(write_foreign_plan_hash)]
fn rejects_a_record_it_cannot_vouch_for(#[case] corrupt: fn(&std::path::Path)) {
    let (dir, store) = store();
    let rec = record();
    store.create(&rec).unwrap();
    corrupt(&record_path(&dir, &rec.transaction_id));
    let err = store.load(&rec.transaction_id).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
}

fn write_not_json(path: &std::path::Path) {
    std::fs::write(path, b"not json").unwrap();
}

fn write_unknown_schema(path: &std::path::Path) {
    let mut json = read_json(path);
    json["schema"] = serde_json::json!(99);
    std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
}

fn write_foreign_plan_hash(path: &std::path::Path) {
    let mut json = read_json(path);
    json["plan_hash"] = serde_json::json!("00".repeat(32));
    std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn record_path(dir: &TempDir, id: &TransactionId) -> std::path::PathBuf {
    dir.path()
        .join("state/transactions")
        .join(id.to_string())
        .join("transaction.json")
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
    assert_eq!(
        store.load(&id).unwrap(),
        rec,
        "a fresh record reads back intact"
    );
    for expected in 0..3 {
        rec.touch();
        store.compare_and_swap(expected, &rec).unwrap();
        let on_disk: TransactionRecord =
            serde_json::from_slice(&std::fs::read(record_path(&dir, &id)).unwrap()).unwrap();
        assert_eq!(on_disk, rec);
        assert_eq!(
            store.load(&id).unwrap(),
            rec,
            "a replaced record reads back intact"
        );
    }
    let mut entries: Vec<String> = std::fs::read_dir(record_path(&dir, &id).parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["transaction.json", "transaction.lock"]);
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
fn a_journal_cannot_be_relocated_under_another_transaction_identity() {
    let (root, store) = store();
    let original = record();
    store.create(&original).unwrap();
    let other = TransactionId::new_v7();
    std::fs::rename(
        root.path()
            .join("state/transactions")
            .join(original.transaction_id.to_string()),
        root.path()
            .join("state/transactions")
            .join(other.to_string()),
    )
    .unwrap();
    assert!(matches!(
        store.load(&other),
        Err(StoreError::Corrupt(CorruptReason::IdentityMismatch))
    ));
}

enum ReceiptMutation {
    Destination,
    Content,
    Object,
}

#[rstest]
#[case(ReceiptMutation::Destination)]
#[case(ReceiptMutation::Content)]
#[case(ReceiptMutation::Object)]
fn receipts_must_describe_the_planned_file(#[case] mutation: ReceiptMutation) {
    let (_root, store) = store();
    let settled = zup_transaction::TransactionCoordinator::new(store)
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            sample_plan(),
        )
        .unwrap()
        .execute(
            &mut common::fake::FakeExecutor::new(),
            &common::Publisher::default(),
        )
        .unwrap();
    let mut record = settled.record;
    let receipt = record
        .nodes
        .values_mut()
        .find_map(|state| match state {
            NodeState::Verified { receipt } | NodeState::Applied { receipt }
                if matches!(
                    **receipt,
                    zup_transaction::OperationReceipt::CreateFile { .. }
                ) =>
            {
                Some(receipt)
            }
            _ => None,
        })
        .unwrap();
    if let zup_transaction::OperationReceipt::CreateFile {
        destination: path,
        installed_size,
        object,
        ..
    } = receipt.as_mut()
    {
        match mutation {
            ReceiptMutation::Destination => *path = r"C:\foreign\owned-by-someone-else".into(),
            ReceiptMutation::Content => *installed_size += 1,
            ReceiptMutation::Object => {
                *object = zup_core::NativeObjectIdentity::Windows {
                    volume: 1,
                    file: [99; 16],
                }
            }
        }
    }
    assert_eq!(record.validate(), Err(CorruptReason::InvalidReceipt));
}
