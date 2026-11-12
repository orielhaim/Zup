mod common;
use common::{Publisher, effects, fake::FakeExecutor, sample_app_id, sample_version};
use zup_core::{
    DataCompatibility, PersistentDirectory, Privilege, ResourceId, SelectedScope, Template,
};
use zup_plan::{DeploymentEffects, PersistentDirectoryOperation, compile_deployment};
use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome};

fn data_effects(schema: u32, previous: Option<&zup_plan::InstallationRecord>) -> DeploymentEffects {
    let mut effects = effects();
    let directory = PersistentDirectory {
        id: ResourceId::new("database").unwrap(),
        location: zup_core::PersistentLocation::Path {
            path: common::tpath(r"C:\Data\Acme"),
            access: zup_core::PersistentAccess::Owner,
        },
        compatibility: DataCompatibility::Schema {
            writes: schema.try_into().unwrap(),
            reads: [1.try_into().unwrap()].into(),
        },
    };
    effects.requirements.data.push(
        directory
            .map_path(|path| Template::parse(path.as_str()))
            .unwrap(),
    );
    effects.observed = previous.map(|record| record.current.selection());
    effects.observed_transaction = previous.map(|record| record.current.transaction);
    let intent = zup_plan::PersistentDirectoryIntent {
        installation: effects.installation,
        previous: previous
            .and_then(|record| record.data.get(&directory.id))
            .cloned(),
        binding: zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding {
            path: directory.location.path().unwrap().clone(),
            writers: Vec::new(),
        }),
        directory,
        privilege: Privilege::User,
    };
    for stage in [
        zup_plan::PersistentDirectoryStage::Prepare,
        zup_plan::PersistentDirectoryStage::Access,
    ] {
        effects.data.push(PersistentDirectoryOperation {
            intent: intent.clone(),
            stage,
        });
    }
    effects
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn data_schema_controls_downgrade_and_failure_compensation(#[case] fail_after_exposure: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let publisher = Publisher::default();
    let first = TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            compile_deployment(&data_effects(1, None)).unwrap(),
        )
        .unwrap();
    assert_eq!(
        first
            .execute(&mut FakeExecutor::new(), &publisher)
            .unwrap()
            .outcome,
        TransactionOutcome::Committed
    );
    let previous = publisher.0.lock().unwrap().clone().unwrap();

    let mut effects = data_effects(2, Some(&previous));
    effects
        .files
        .push(common::file("application.exe", b"new application"));
    let plan = compile_deployment(&effects).unwrap();
    let exposure = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
        .unwrap()
        .id
        .clone();
    let mut executor = FakeExecutor::new();
    if fail_after_exposure {
        executor.fail_verify_operation = Some(exposure);
    }
    let open = TransactionCoordinator::new(store)
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap();
    let settled = open.execute(&mut executor, &publisher).unwrap();
    if fail_after_exposure {
        assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
        assert!(executor.rolled_back.is_empty());
        assert!(settled.record.rollback_failure.is_some());
        let assessment =
            zup_transaction::assess_recovery(&settled.record, &mut FakeExecutor::new()).unwrap();
        assert!(assessment.complete_forward.is_none());
        assert!(assessment.blockers.iter().any(
            |blocker| blocker.cause == zup_transaction::RecoveryCause::IncompatibleDataExposure
        ));
    } else {
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
        let current = publisher.0.lock().unwrap().clone().unwrap();
        assert!(compile_deployment(&data_effects(1, Some(&current))).is_err());
    }
}
