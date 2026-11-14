mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
mod common;

use zup_core::CommandSpec;
use zup_core::{
    InstallationId, NativeServiceRegistration, Privilege, ResourceKey, ServiceId, ServiceStart,
    Target, TargetPath,
};
use zup_plan::{
    DeploymentEffects, IntegrationChangeKind, NativeServiceOperation, NodeKind,
    ObservedServiceState, ServiceActivation, ServiceOperation, compile_deployment,
};
use zup_transaction::NativeServiceReceipt;

fn service() -> NativeServiceOperation {
    let execution = zup_core::ServiceExecution::WindowsJob {
        account: zup_core::WindowsServiceAccount::LocalService,
    };
    NativeServiceOperation::Configure {
        service: Box::new(ServiceOperation {
            state_directories: Vec::new(),
            registration: NativeServiceRegistration::Scm {
                installation: InstallationId::new_v7(),
            },
            key: ResourceKey::Service {
                id: ServiceId::new("worker").unwrap(),
            },
            id: "worker".into(),
            name: "worker".into(),
            display_name: "Worker".into(),
            command: CommandSpec::new(
                TargetPath::new(Target::WINDOWS_X64, r"C:\Application\worker.exe").unwrap(),
                Vec::new(),
            ),
            lifecycle: zup_core::ServiceLifecycle {
                ..Default::default()
            }
            .resolved(&execution),
            execution,
            start: ServiceStart::Disabled,
            readiness: Default::default(),
            failure: Default::default(),
            privilege: Privilege::System,
            kind: IntegrationChangeKind::Create,
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            previous: ObservedServiceState::Absent,
            conflict: None,
        }),
        previous: None,
        activation: ServiceActivation::Declared,
    }
}

#[test]
fn service_receipts_bind_configuration_ownership_and_complete_intent() {
    let operation = service();
    let ResourceKey::Service { id } = operation.key() else {
        unreachable!()
    };
    let receipt = NativeServiceReceipt {
        intent: operation.fingerprint(),
        configuration: operation.configurations().1,
        registration: zup_core::ObservedServiceRegistration::Scm {
            claim: operation.registration().claim(id.clone()).unwrap(),
        },
    };
    assert!(receipt.matches(&operation));
    let mut foreign = receipt.clone();
    foreign.registration = zup_core::ObservedServiceRegistration::Unclaimed;
    assert!(!foreign.matches(&operation));
    let mut changed = operation.clone();
    let NativeServiceOperation::Configure { service, .. } = &mut changed else {
        unreachable!()
    };
    service
        .requires_files
        .push(service.command.executable.clone());
    assert!(!receipt.matches(&changed));
    let mut absent = receipt;
    absent.configuration = zup_plan::ServiceState::Absent;
    assert!(!absent.matches(&operation));
}

#[test]
fn service_effects_require_protected_history_and_explicit_retirement() {
    let operation = service();
    let NativeServiceRegistration::Scm { installation } = operation.registration() else {
        unreachable!()
    };
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            Target::WINDOWS_X64,
            zup_core::SelectedScope::Machine,
            common::sample_app_id().as_str(),
            &common::sample_version().to_string(),
        ),
    );
    effects.install_directory =
        Some(TargetPath::new(Target::WINDOWS_X64, r"C:\Application").unwrap());
    effects.native_services.push(operation.clone());
    let plan = compile_deployment(&effects).unwrap();
    assert!(
        plan.nodes
            .iter()
            .any(|node| matches!(node.kind, NodeKind::NativeService { .. }))
    );
    let contract = operation.contract();
    let mut unchanged = operation.clone();
    let NativeServiceOperation::Configure { service, .. } = &mut unchanged else {
        unreachable!()
    };
    service.kind = IntegrationChangeKind::NoOp;
    service.previous = ObservedServiceState::Service {
        state_directories: Vec::new(),
        native_timeouts: zup_core::NativeServiceTimeouts::Scm,
        dependencies: service.dependency_names.clone(),
        failure: service.failure,
        execution: service.execution.clone(),
        display_name: service.display_name.clone(),
        command: Box::new(service.command.clone()),
        start: service.start,
        readiness: service.readiness,
        runtime_state: Some(zup_plan::ServiceRuntimeState::Stopped),
    };
    service
        .requires_files
        .push(service.command.executable.clone());
    assert!(
        unchanged
            .validate(installation, Target::WINDOWS_X64)
            .is_err()
    );
    let NativeServiceOperation::Configure { previous, .. } = &mut unchanged else {
        unreachable!()
    };
    *previous = Some(Box::new(contract.clone()));
    unchanged
        .validate(installation, Target::WINDOWS_X64)
        .unwrap();
    let removal = NativeServiceOperation::Remove {
        key: operation.key().clone(),
        service: Box::new(contract),
        activation: ServiceActivation::Declared,
    };
    effects.native_services = vec![removal];
    assert!(compile_deployment(&effects).is_err());
    effects.retired_keys.push(operation.key().clone());
    compile_deployment(&effects).unwrap();
}

#[test]
fn generation_verification_guards_registration_and_runtime_with_the_same_service_key() {
    let mut operation = service();
    let NativeServiceOperation::Configure {
        service,
        activation,
        ..
    } = &mut operation
    else {
        unreachable!()
    };
    service.start = ServiceStart::Manual;
    *activation = ServiceActivation::Inhibited;
    let NativeServiceRegistration::Scm { installation } = operation.registration() else {
        unreachable!()
    };
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            Target::WINDOWS_X64,
            zup_core::SelectedScope::Machine,
            common::sample_app_id().as_str(),
            &common::sample_version().to_string(),
        ),
    );
    let directory = TargetPath::new(Target::WINDOWS_X64, r"C:\Application").unwrap();
    effects.install_directory = Some(directory.clone());
    let contract = operation.contract();
    effects
        .service_controls
        .push(zup_plan::ServiceControlOperation {
            requires_files: Vec::new(),
            registration: operation.registration(),
            intent: zup_plan::ServiceControlIntent::Transition,
            key: operation.key().clone(),
            native_name: contract.name.clone(),
            privilege: Privilege::System,
            stage: zup_plan::ServiceControlStage::Restore,
            before_configuration: operation.configurations().1,
            configuration: contract.installed.as_ref().clone(),
            owned_configuration: contract.installed.as_ref().clone(),
            before: zup_plan::ServiceTerminalState::Stopped,
            after: zup_plan::ServiceTerminalState::Running,
            manager_generation: "boot".into(),
            before_execution_generation: "stopped".into(),
            process_scope: zup_plan::ServiceScopeObservation::WindowsJob {
                boot_sequence: 1,
                state: zup_plan::WindowsJobState::Absent,
            },
            timeouts: contract.timeouts,
            dependencies: Vec::new(),
        });
    effects.native_services.push(operation.clone());
    let materialization = zup_core::MaterializationId::new_v7();
    effects.generations.push(zup_plan::GenerationIntent {
        sources: Default::default(),
        preparation: zup_plan::GenerationPreparation::Materialize,
        materialization: zup_core::MaterializationSpec::new(
            installation,
            materialization,
            directory
                .join(format!(
                    "{}/{materialization}",
                    zup_core::DEPLOYMENTS_DIRECTORY
                ))
                .unwrap(),
            zup_core::GenerationDescriptor::new(Target::WINDOWS_X64, Vec::new()).unwrap(),
        )
        .unwrap(),
        publication: zup_plan::GenerationPublication::Application {
            previous: None,
            revision: zup_core::ActivationRevision::FIRST,
        },
        privilege: Privilege::System,
        exposures: std::collections::BTreeSet::from([operation.key().clone()]),
    });
    let plan = compile_deployment(&effects).unwrap();
    let seal = effects
        .generations
        .first()
        .unwrap()
        .operation_id(zup_plan::GenerationOperation::Seal);
    for exposed in plan.nodes.iter().filter(|node| {
        matches!(
            node.kind,
            NodeKind::NativeService { .. } | NodeKind::ServiceControl { .. }
        )
    }) {
        let mut unsafe_plan = plan.clone();
        unsafe_plan
            .dependencies
            .retain(|edge| !(edge.from == seal && edge.to == exposed.id));
        assert!(unsafe_plan.validate().is_err());
    }
}

#[test]
fn committed_service_contracts_publish_refresh_and_retire_under_one_logical_identity() {
    use zup_transaction::{
        FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome, validate_ownership,
    };
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let publisher = common::Publisher::default();
    let mut operation = service();
    let NativeServiceRegistration::Scm { installation } = operation.registration() else {
        unreachable!()
    };
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            Target::WINDOWS_X64,
            zup_core::SelectedScope::Machine,
            common::sample_app_id().as_str(),
            &common::sample_version().to_string(),
        ),
    );
    effects.install_directory =
        Some(TargetPath::new(Target::WINDOWS_X64, r"C:\Application").unwrap());
    for step in 0..3 {
        let previous = publisher.0.lock().unwrap().clone();
        effects.observed = previous.as_ref().map(|record| record.current.selection());
        effects.observed_transaction = previous.as_ref().map(|record| record.current.transaction);
        if let Some(previous) = previous.as_ref() {
            let zup_plan::OwnedResource::Service(owned) =
                previous.current.resources.get(operation.key()).unwrap()
            else {
                unreachable!()
            };
            if step == 1 {
                let NativeServiceOperation::Configure {
                    service, previous, ..
                } = &mut operation
                else {
                    unreachable!()
                };
                service.kind = IntegrationChangeKind::NoOp;
                service.previous = ObservedServiceState::Service {
                    state_directories: Vec::new(),
                    native_timeouts: zup_core::NativeServiceTimeouts::Scm,
                    dependencies: Vec::new(),
                    failure: service.failure,
                    execution: service.execution.clone(),
                    display_name: service.display_name.clone(),
                    command: Box::new(service.command.clone()),
                    start: service.start,
                    readiness: service.readiness,
                    runtime_state: Some(zup_plan::ServiceRuntimeState::Stopped),
                };
                service
                    .requires_files
                    .push(service.command.executable.clone());
                *previous = Some(Box::new(owned.clone()));
            } else {
                operation = NativeServiceOperation::Remove {
                    key: operation.key().clone(),
                    service: Box::new(owned.clone()),
                    activation: ServiceActivation::Declared,
                };
                effects.retired_keys.push(operation.key().clone());
                effects.transition = zup_plan::DeploymentTransition::Remove;
            }
        }
        effects.native_services = vec![operation.clone()];
        let plan = compile_deployment(&effects).unwrap();
        validate_ownership(
            &plan,
            zup_core::SelectedScope::Machine,
            previous.as_ref(),
            &effects.retired_keys.iter().cloned().collect(),
            None,
        )
        .unwrap();
        let settled = TransactionCoordinator::new(store.clone())
            .begin(
                common::sample_app_id(),
                zup_core::SelectedScope::Machine,
                common::sample_version(),
                plan,
            )
            .unwrap()
            .execute(&mut common::fake::FakeExecutor::new(), &publisher)
            .unwrap();
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
        let installed = publisher.0.lock().unwrap().clone().unwrap();
        assert_eq!(installed.retained.len(), step);
        if step < 2 {
            assert_eq!(
                installed.current.resources.get(operation.key()),
                Some(&zup_plan::OwnedResource::Service(operation.contract()))
            );
        } else {
            assert!(installed.current.resources.is_empty());
            assert_eq!(installed.state, zup_plan::InstallationState::Removed);
        }
    }
}

fn source_effects(remove: bool) -> (DeploymentEffects, NativeServiceOperation) {
    let mut operation = service();
    let NativeServiceOperation::Configure { service, .. } = &mut operation else {
        unreachable!()
    };
    service.registration = NativeServiceRegistration::SourceFile;
    service.execution = zup_core::ServiceExecution::SystemdDynamicUser;
    service.command.executable =
        TargetPath::new(Target::LINUX_X64, "/opt/application/worker").unwrap();
    service.lifecycle = zup_core::ServiceLifecycle::default().resolved(&service.execution);
    service.name = "worker.service".into();
    let installation = InstallationId::new_v7();
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            Target::LINUX_X64,
            zup_core::SelectedScope::Machine,
            common::sample_app_id().as_str(),
            &common::sample_version().to_string(),
        ),
    );
    effects.install_directory =
        Some(TargetPath::new(Target::LINUX_X64, "/opt/application").unwrap());
    let source = operation.source_key().unwrap();
    let destination = TargetPath::new(
        Target::LINUX_X64,
        "/usr/local/lib/systemd/system/worker.service",
    )
    .unwrap();
    if remove {
        operation = NativeServiceOperation::Remove {
            key: operation.key().clone(),
            service: Box::new(operation.contract()),
            activation: ServiceActivation::Declared,
        };
        effects.removals.push(zup_plan::FileRemoval {
            key: source.clone(),
            kind: zup_plan::RemovalKind::RemoveOwned,
            scope: zup_core::SelectedScope::Machine,
            privilege: Privilege::System,
            destination,
            size: 6,
            sha256: zup_core::hash_bytes(b"source"),
            instance: zup_core::FixedFileInstance {
                origin: zup_core::TransactionId::new_v7(),
                object: zup_core::NativeObjectIdentity::Linux {
                    device: 1,
                    inode: 2,
                },
            },
            created_directories: Vec::new(),
        });
        effects.retired_keys = vec![source, operation.key().clone()];
    } else {
        effects.files.push(zup_plan::FileOperation {
            key: source,
            source_relative: zup_core::RelativePath::new("__zup_generated__/services/worker")
                .unwrap(),
            destination,
            precondition: zup_plan::FilePrecondition::Absent,
            expected_size: 6,
            expected_sha256: zup_core::hash_bytes(b"source"),
            privilege: Privilege::System,
            kind: zup_plan::FileOperationKind::Create,
            executable: false,
            conflict: None,
            replacement: zup_core::FixedReplacement::Atomic,
        });
    }
    effects.native_services.push(operation.clone());
    (effects, operation)
}

#[rstest::rstest]
#[case::configure(false)]
#[case::remove(true)]
fn native_service_source_restoration_precedes_manager_compensation(#[case] remove: bool) {
    let (mut effects, operation) = source_effects(remove);
    let executable = TargetPath::new(Target::LINUX_X64, "/opt/application/worker").unwrap();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("worker").unwrap(),
            },
        },
    };
    if remove {
        let mut payload = effects.removals[0].clone();
        payload.key = key.clone();
        payload.destination = executable.clone();
        payload.instance.object = zup_core::NativeObjectIdentity::Linux {
            device: 1,
            inode: 3,
        };
        effects.removals.push(payload);
        effects.retired_keys.push(key);
    } else {
        let mut payload = effects.files[0].clone();
        payload.key = key;
        payload.source_relative = zup_core::RelativePath::new("worker").unwrap();
        payload.destination = executable.clone();
        payload.executable = true;
        effects.files.push(payload);
    }
    let plan = compile_deployment(&effects).unwrap();
    let source = operation.source_node(&plan.nodes).unwrap().unwrap();
    let payload = plan
        .nodes
        .iter()
        .find(|node| node.mutates_file(&executable))
        .unwrap();
    assert!(
        plan.execution_order
            .iter()
            .position(|id| id == &payload.id)
            .unwrap()
            < plan
                .execution_order
                .iter()
                .position(|id| id == &source.id)
                .unwrap()
    );
    for order in [&plan.execution_order, &plan.rollback_order] {
        assert!(
            order.iter().position(|id| id == &source.id).unwrap()
                < order.iter().position(|id| id == &operation.id()).unwrap()
        );
    }
    let mut missing_source = effects.clone();
    missing_source.files.clear();
    missing_source.removals.clear();
    assert!(compile_deployment(&missing_source).is_err());
    let mut orphan_source = effects;
    orphan_source.native_services.clear();
    assert!(compile_deployment(&orphan_source).is_err());
}

struct FailedNativeConfiguration {
    inner: common::fake::FakeExecutor,
    interrupt_compensation: bool,
}

impl zup_transaction::OperationExecutor for FailedNativeConfiguration {
    type Error = String;
    fn prepare(&mut self, node: &zup_plan::DeploymentNode) -> Result<(), String> {
        self.inner.prepare(node)
    }
    fn apply(
        &mut self,
        node: &zup_plan::DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::OperationReceipt, String> {
        if matches!(node.kind, NodeKind::NativeService { .. }) {
            return Err("native configuration failed before mutation".into());
        }
        self.inner.apply(node, transaction)
    }
    fn verify(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        self.inner.verify(node, receipt)
    }
    fn rollback(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        if self.interrupt_compensation
            && matches!(
                receipt,
                zup_transaction::OperationReceipt::NativeServiceCompensation { .. }
            )
        {
            self.interrupt_compensation = false;
            return Err("native compensation interrupted".into());
        }
        self.inner.rollback(node, receipt)
    }
    fn reconcile(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::ReconcileResult, String> {
        self.inner.reconcile(node, receipt, transaction)
    }
    fn reconcile_rollback(
        &mut self,
        _: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<zup_transaction::RollbackReconcileResult, String> {
        Ok(
            if matches!(
                receipt,
                zup_transaction::OperationReceipt::NativeServiceCompensation { .. }
            ) {
                zup_transaction::RollbackReconcileResult::Retry
            } else {
                zup_transaction::RollbackReconcileResult::Ambiguous
            },
        )
    }
}

#[rstest::rstest]
#[case::compensate(false)]
#[case::recover_interrupted_compensation(true)]
fn restored_service_sources_leave_a_durable_native_compensation_obligation(
    #[case] interrupt_compensation: bool,
) {
    use zup_transaction::{
        FilesystemTransactionStore, NodeState, TransactionCoordinator, TransactionOutcome,
        TransactionStore,
    };
    let (effects, operation) = source_effects(false);
    let plan = compile_deployment(&effects).unwrap();
    let source = operation
        .source_node(&plan.nodes)
        .unwrap()
        .unwrap()
        .id
        .clone();
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let publisher = common::Publisher::default();
    let mut executor = FailedNativeConfiguration {
        inner: common::fake::FakeExecutor::new(),
        interrupt_compensation,
    };
    let settled = TransactionCoordinator::new(store.clone())
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::Machine,
            common::sample_version(),
            plan,
        )
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    let record = if interrupt_compensation {
        assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
        let durable = store.load(&settled.record.transaction_id).unwrap();
        assert!(
            matches!(durable.nodes.get(&operation.id()), Some(NodeState::RollingBack { receipt })
            if matches!(receipt.as_ref(), zup_transaction::OperationReceipt::NativeServiceCompensation { intent } if *intent == operation.fingerprint()))
        );
        let (record, outcome) =
            zup_transaction::recover(durable, &store, &mut executor, &publisher).unwrap();
        assert_eq!(outcome, TransactionOutcome::RolledBack);
        record
    } else {
        assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
        settled.record
    };
    assert!(publisher.0.lock().unwrap().is_none());
    assert!(
        record
            .nodes
            .get(&operation.id())
            .is_some_and(NodeState::is_rolled_back)
    );
    assert!(
        executor.inner.position("rollback", &source).unwrap()
            < executor
                .inner
                .position("rollback", &operation.id())
                .unwrap()
    );
}
