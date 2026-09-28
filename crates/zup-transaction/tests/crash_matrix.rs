//! The crash matrix: every crash point, recovered.
//!
//! The other crash tests in this package are hand-picked cases - a crash after
//! `Prepared` persisted, a crash after `Running` persisted, a crash after an
//! apply but before `Applied` persisted. Each names a situation somebody thought
//! of, which is exactly what a matrix replaces.
//!
//! So this file derives the crash points instead. For each executor call in turn
//! it kills the transaction **at that call** - with a panic, because a panic is
//! the difference between the coordinator rolling back and the process
//! disappearing - and then recovers whatever the store still holds. A crash can
//! only ever leave the machine in a state that was durably written, so the
//! crashes this file injects *are* the universe of crash points. A crash state
//! this file does not test is one this file never produced, not one it forgot.
//!
//! The assertion is the invariant, stated once and checked at every point:
//!
//! > Recovery ends in exactly one of three settled states, each with properties
//! > of its own, and the record it returns is still a *legal* record.
//!
//! Recovery is allowed to be conservative. What it may not do is end unsettled,
//! leave a rolled-back transaction with applied work still recorded as applied,
//! report a phase its own node states contradict, or settle into a record that
//! fails validation - that last one trades an unfinished transaction for a corrupt
//! one, which is worse than either.

mod common;
mod fake;

use common::{sample_app_id, sample_input, sample_version};
use fake::FakeExecutor;
use tempfile::TempDir;
use zup_transaction::{
    FilesystemTransactionStore, NodeState, OperationExecutor, OperationReceipt,
    TransactionCoordinator, TransactionId, TransactionNode, TransactionOutcome, TransactionPhase,
    TransactionRecord, TransactionStore, compile_transaction, recover,
};

/// An executor that dies on its *n*-th call.
struct CrashingExecutor {
    calls: usize,
    /// Which call kills the process. `usize::MAX` never fires.
    die_at: usize,
}

impl CrashingExecutor {
    fn new(die_at: usize) -> Self {
        Self { calls: 0, die_at }
    }

    fn reached(&mut self) {
        self.calls += 1;
        if self.calls == self.die_at {
            panic!("injected crash at executor call {}", self.calls);
        }
    }
}

impl OperationExecutor for CrashingExecutor {
    type Error = String;

    fn prepare(&mut self, _operation: &TransactionNode) -> Result<(), Self::Error> {
        self.reached();
        Ok(())
    }

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        self.reached();
        // A real receipt for the node, not a blanket `Control`: the record's own
        // validation refuses a control receipt on a file mutation, and a fake that
        // produced records the store would reject would test nothing.
        Ok(fake::receipt_for(operation))
    }

    fn verify(
        &mut self,
        _operation: &TransactionNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.reached();
        Ok(())
    }

    fn rollback(
        &mut self,
        _operation: &TransactionNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.reached();
        Ok(())
    }

    fn reconcile(
        &mut self,
        _operation: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
    ) -> Result<zup_transaction::ReconcileResult, Self::Error> {
        self.reached();
        Ok(zup_transaction::ReconcileResult::NotApplied)
    }
}

/// The properties each settled outcome has to have.
fn assert_settled(record: &TransactionRecord, outcome: TransactionOutcome) {
    match outcome {
        TransactionOutcome::Committed => {
            assert_eq!(
                record.phase,
                TransactionPhase::Committed,
                "a committed record must say so"
            );
        }
        TransactionOutcome::RolledBack => {
            assert_eq!(
                record.phase,
                TransactionPhase::RolledBack,
                "a rolled-back record must say so"
            );
            for (id, state) in &record.nodes {
                assert!(
                    matches!(
                        state,
                        NodeState::Pending | NodeState::Failed | NodeState::RolledBack
                    ),
                    "a rolled-back transaction left `{id}` as {state:?}"
                );
            }
        }
        TransactionOutcome::RecoveryRequired => {
            assert_eq!(
                record.phase,
                TransactionPhase::RecoveryRequired,
                "a record that needs a human must say so, and a record that does not must not \
                 claim it"
            );
        }
    }
    record
        .validate()
        .unwrap_or_else(|error| panic!("recovery settled into an invalid record: {error}"));
}

/// A fingerprint of the durable state, so two crash points that left the machine
/// in the same place are counted once.
fn fingerprint(record: &TransactionRecord) -> String {
    let mut states: Vec<String> = record
        .nodes
        .values()
        .map(|state| format!("{state:?}"))
        .collect();
    states.sort();
    format!("{:?}|{}", record.phase, states.join(","))
}

/// Build a fresh transaction of `input` in a fresh store.
fn fresh(
    input: &zup_transaction::TransactionInput,
) -> (
    TempDir,
    FilesystemTransactionStore,
    TransactionCoordinator<FilesystemTransactionStore>,
    TransactionId,
) {
    let dir = TempDir::new().expect("a temp dir");
    let store = FilesystemTransactionStore::new(dir.path());
    let coordinator = TransactionCoordinator::new(FilesystemTransactionStore::new(dir.path()));
    let id = TransactionId::new_v7();
    let record = TransactionRecord::new(
        id,
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        compile_transaction(input).expect("a plan"),
    );
    store.create(&record).expect("create");
    (dir, store, coordinator, id)
}

/// Kill the transaction at every executor call in turn, and recover what each kill
/// left behind.
fn sweep(label: &str, input: zup_transaction::TransactionInput) {
    let calls = {
        let (_dir, store, coordinator, id) = fresh(&input);
        let mut counter = CrashingExecutor::new(usize::MAX);
        let record = store.load(&id).expect("a durable record");
        coordinator
            .execute(record, &mut counter)
            .unwrap_or_else(|error| panic!("{label}: an uninterrupted run failed: {error}"));
        counter.calls
    };
    assert!(
        calls >= 8,
        "{label}: only {calls} executor calls to crash at"
    );

    let mut outcomes = Vec::new();
    let mut distinct = std::collections::BTreeSet::new();
    for die_at in 1..=calls {
        let (_dir, store, coordinator, id) = fresh(&input);
        let record = store.load(&id).expect("a durable record");
        let mut executor = CrashingExecutor::new(die_at);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coordinator.execute(record, &mut executor)
        }));
        if crashed.is_ok() {
            // Fewer calls than expected: the run finished before reaching this
            // one, so this index is not a crash point.
            continue;
        }
        // What the machine holds now, read back from the store rather than taken
        // from the coordinator, because that is what a restart would find.
        let persisted = store
            .load(&id)
            .expect("the crashed record is durable and readable");
        let crashed_phase = persisted.phase;
        let crossed_commit_intent = matches!(
            crashed_phase,
            TransactionPhase::Applying | TransactionPhase::Committed
        );
        distinct.insert(fingerprint(&persisted));

        let (_, outcome) = recover(persisted, &store, &mut FakeExecutor::new())
            .unwrap_or_else(|error| panic!("{label}: crash {die_at} did not recover: {error}"));
        let settled = store.load(&id).unwrap_or_else(|error| {
            panic!("{label}: the store is unreadable after {die_at}: {error}")
        });
        assert_settled(&settled, outcome);

        // The one implication worth stating precisely, because it is the whole
        // meaning of commit intent: recovery may roll a transaction *forward*, and
        // only if the durable record had already crossed the barrier that says
        // "finish this plan". A crash before that barrier must abandon the work
        // rather than adopt it, because a user who pressed cancel should not find
        // the installation half-done because the process died at an unlucky moment.
        //
        // The converse also holds, and it is the one that would be a real bug: a
        // record that crossed commit intent and did not recover to `Committed` has
        // abandoned work the machine had promised to finish.
        assert_eq!(
            outcome == TransactionOutcome::Committed,
            crossed_commit_intent,
            "{label}: crash {die_at} left the record in {crashed_phase:?} and recovered to \
             {outcome:?}; a record that crossed commit intent has to finish, and one that did \
             not has to be abandoned"
        );
        outcomes.push(outcome);
    }

    assert!(
        outcomes.len() + 1 >= calls,
        "{label}: {} of {calls} crash points fired",
        outcomes.len()
    );
    assert!(
        distinct.len() >= 3,
        "{label}: {outcomes:?} crash points produced only {} distinct durable state(s), so most \
         of them are the same crash",
        distinct.len()
    );
    assert_eq!(
        outcomes.last(),
        Some(&TransactionOutcome::Committed),
        "{label}: the last crash point recovered to {outcomes:?}"
    );
    println!(
        "{label}: {} crash points, {} distinct states, {outcomes:?}",
        outcomes.len(),
        distinct.len()
    );
}

/// All three plans, in one test.
///
/// One test rather than three because injecting a crash means panicking, and
/// suppressing the panic output means replacing the process-wide hook - which
/// three tests running in parallel would race on. The label in every message
/// says which plan a failure came from.
#[test]
fn every_crash_point_of_every_plan_shape_recovers() {
    // A panic prints a backtrace by default, and this injects dozens of them.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    sweep("create-apply", sample_input());
    // A removal is the one node kind whose rollback has to *restore* rather than
    // delete, and whose plan crosses commit intent earlier than a create does.
    sweep("removal", common::removal_input());
    // A longer dependency chain, so there is more than one ordering to get wrong.
    sweep("chain", common::chain_input());
    std::panic::set_hook(hook);
}
