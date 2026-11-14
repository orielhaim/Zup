mod common;

use common::{effects, fake::FakeExecutor, sample_app_id, sample_version};
use std::sync::atomic::{AtomicBool, Ordering};
use zup_core::SelectedScope;
use zup_plan::{RetentionChange, compile_deployment};
use zup_transaction::{
    FilesystemTransactionStore, InstallationPublisher, TransactionCoordinator, TransactionOutcome,
};

#[derive(Default)]
struct Publisher {
    durable: common::Publisher,
    refuse: AtomicBool,
}

impl InstallationPublisher for Publisher {
    type Error = String;
    fn read_current(
        &self,
        record: &zup_transaction::TransactionRecord,
    ) -> Result<Option<zup_plan::InstallationRecord>, String> {
        self.durable.read_current(record)
    }

    fn prepare(
        &self,
        record: &zup_transaction::TransactionRecord,
    ) -> Result<zup_plan::InstallationRecord, String> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err("publication unavailable".into());
        }
        self.durable.prepare(record)
    }
    fn publish(
        &self,
        record: &zup_transaction::TransactionRecord,
        observed: &zup_transaction::PublicationObservation,
    ) -> Result<zup_plan::InstallationRecord, String> {
        self.durable.publish(record, observed)
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn retirement_is_authorized_by_retained_identity_and_commits_with_the_record(
    #[case] interrupt_publication: bool,
) {
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let publisher = Publisher::default();
    for _ in 0..2 {
        let previous = publisher.durable.0.lock().unwrap().clone();
        let mut input = effects();
        input.observed = previous.as_ref().map(|record| record.current.selection());
        input.observed_transaction = previous.as_ref().map(|record| record.current.transaction);
        let open = TransactionCoordinator::new(store.clone())
            .begin(
                sample_app_id(),
                SelectedScope::User,
                sample_version(),
                compile_deployment(&input).unwrap(),
            )
            .unwrap();
        assert_eq!(
            open.execute(&mut FakeExecutor::new(), &publisher)
                .unwrap()
                .outcome,
            TransactionOutcome::Committed
        );
    }
    let previous = publisher.durable.0.lock().unwrap().clone().unwrap();
    let selected = previous.retained[0].clone();
    let mut input = effects();
    input.observed = Some(previous.current.selection());
    input.observed_transaction = Some(previous.current.transaction);
    input.transition = zup_plan::DeploymentTransition::Maintain;
    input.retention = RetentionChange::keeping_previous(&previous, 0);
    let plan = compile_deployment(&input).unwrap();
    let mut forged = plan.clone();
    let selection = forged
        .nodes
        .iter_mut()
        .find_map(|node| match &mut node.kind {
            zup_plan::NodeKind::Retention { selection } => Some(selection),
            _ => None,
        })
        .unwrap();
    selection.retire[0].transaction = zup_core::TransactionId::new_v7();
    assert!(
        zup_transaction::validate_ownership(
            &forged,
            SelectedScope::User,
            Some(&previous),
            &Default::default(),
            None
        )
        .is_err()
    );
    publisher
        .refuse
        .store(interrupt_publication, Ordering::SeqCst);
    let settled = TransactionCoordinator::new(store.clone())
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut FakeExecutor::new(), &publisher)
        .unwrap();
    let record = if interrupt_publication {
        assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(
            publisher.durable.0.lock().unwrap().as_ref().unwrap(),
            &previous
        );
        publisher.refuse.store(false, Ordering::SeqCst);
        let (record, outcome) =
            zup_transaction::recover(settled.record, &store, &mut FakeExecutor::new(), &publisher)
                .unwrap();
        assert_eq!(outcome, TransactionOutcome::Committed);
        record
    } else {
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
        settled.record
    };
    let current = publisher.durable.0.lock().unwrap().clone().unwrap();
    assert_eq!(current.current, previous.current);
    assert_ne!(current.commit, previous.commit);
    assert_eq!(current.retired_deployments.len(), 1);
    assert_eq!(current.retired_deployments[0].snapshot, selected);
    assert_eq!(
        current.retired_deployments[0].retired_by,
        record.transaction_id
    );
    assert!(
        !current
            .retained
            .iter()
            .any(|snapshot| snapshot.revision == selected.revision)
    );
    assert!(current.references_transaction(selected.transaction));
}
