mod common;

use zup_core::{Privilege, ResourceKey, ServiceId, ServiceStart};
use zup_plan::{CommandSpec, ServiceRuntimeState, ServiceState};
use zup_transaction::{
    NativeServiceManager, NodeKind, OperationId, ReconcileResult, ServiceControlOperation,
    ServiceControlStage, ServiceObservation, ServiceTerminalState, apply_service_control,
    compile_transaction, reconcile_service_control, rollback_service_control,
    verify_service_control,
};

fn control(id: &str, stage: ServiceControlStage) -> ServiceControlOperation {
    let configuration = ServiceState::Registration {
        execution: zup_core::ServiceExecution::Windows {
            account: zup_core::WindowsServiceAccount::LocalService,
        },
        readiness: Default::default(),
        display_name: id.into(),
        command: CommandSpec::new(common::tpath(r"C:\PF\Acme\a.exe"), Vec::new()),
        start: ServiceStart::Manual,
    };
    ServiceControlOperation {
        key: ResourceKey::Service {
            id: ServiceId::new(id).unwrap(),
        },
        native_name: id.into(),
        privilege: Privilege::System,
        configuration: configuration.clone(),
        owned_configuration: configuration,
        stage,
        before: if stage == ServiceControlStage::Quiesce {
            ServiceTerminalState::Running
        } else {
            ServiceTerminalState::Stopped
        },
        after: if stage == ServiceControlStage::Quiesce {
            ServiceTerminalState::Stopped
        } else {
            ServiceTerminalState::Running
        },
        manager_generation: "boot:manager".into(),
        before_execution_generation: if stage == ServiceControlStage::Quiesce {
            "execution-1"
        } else {
            "stopped"
        }
        .into(),
        timeout_seconds: 30,
        rollback_timeout_seconds: 45,
        dependencies: Vec::new(),
    }
}

#[test]
fn planned_transitions_and_compensation_use_their_respective_deadlines() {
    let intent = control("worker", ServiceControlStage::Quiesce);
    let ServiceState::Registration {
        display_name,
        command,
        start,
        readiness,
        ..
    } = intent.configuration.clone()
    else {
        unreachable!();
    };
    let service = zup_plan::ServiceOperation {
        execution: zup_core::ServiceExecution::Windows {
            account: zup_core::WindowsServiceAccount::LocalService,
        },
        requires_files: Vec::new(),
        readiness,
        key: intent.key.clone(),
        kind: zup_plan::IntegrationChangeKind::UpdateOwned,
        id: "worker".into(),
        name: "worker".into(),
        display_name: display_name.clone(),
        command: command.clone(),
        start,
        lifecycle: zup_core::ServiceLifecycle {
            start_timeout_seconds: 17,
            stop_timeout_seconds: 23,
            ..Default::default()
        },
        privilege: Privilege::System,
        previous: zup_plan::ObservedServiceState::Service {
            execution: zup_core::ServiceExecution::Windows {
                account: zup_core::WindowsServiceAccount::LocalService,
            },
            readiness,
            display_name,
            command,
            start,
            runtime_state: Some(ServiceRuntimeState::Running),
        },
        conflict: None,
    };
    let owned = zup_plan::OwnedResource::Service {
        requires_files: Vec::new(),
        name: "worker".into(),
        privilege: Privilege::System,
        previous: ServiceState::Absent,
        installed: intent.configuration,
    };
    let operations = zup_transaction::plan_service_controls(
        &service,
        Some(&owned),
        true,
        "worker".into(),
        "boot:manager".into(),
        "execution-1".into(),
    )
    .unwrap();
    assert_eq!(operations.len(), 2);
    for operation in operations {
        assert_eq!(
            operation
                .timeout_for(ServiceTerminalState::Running)
                .as_secs(),
            17
        );
        assert_eq!(
            operation
                .timeout_for(ServiceTerminalState::Stopped)
                .as_secs(),
            23
        );
    }
}

#[test]
fn dependency_order_surrounds_all_mutations_and_reverses_for_compensation() {
    let mut input = common::sample_input();
    for stage in [ServiceControlStage::Quiesce, ServiceControlStage::Restore] {
        input.service_controls.push(control("database", stage));
        let mut dependent = control("worker", stage);
        dependent.dependencies.push(ResourceKey::Service {
            id: ServiceId::new("database").unwrap(),
        });
        input.service_controls.push(dependent);
    }
    let plan = compile_transaction(&input).unwrap();
    let position = |name: &str, stage| {
        let node = plan.nodes.iter().find(|node| matches!(&node.kind, NodeKind::ServiceControl { operation } if operation.native_name == name && operation.stage == stage)).unwrap();
        plan.execution_order
            .iter()
            .position(|id| id == &node.id)
            .unwrap()
    };
    let stop_worker = position("worker", ServiceControlStage::Quiesce);
    let stop_database = position("database", ServiceControlStage::Quiesce);
    let start_database = position("database", ServiceControlStage::Restore);
    let start_worker = position("worker", ServiceControlStage::Restore);
    assert!(stop_worker < stop_database && start_database < start_worker);
    for node in plan.nodes.iter().filter(|node| {
        matches!(
            node.kind,
            NodeKind::FileMutation { .. } | NodeKind::BackendOperation { .. }
        )
    }) {
        let mutation = plan
            .execution_order
            .iter()
            .position(|id| id == &node.id)
            .unwrap();
        assert!(stop_database < mutation && mutation < start_database);
    }
    let compensation = |name: &str, stage| {
        let node = plan.nodes.iter().find(|node| matches!(&node.kind, NodeKind::ServiceControl { operation } if operation.native_name == name && operation.stage == stage)).unwrap();
        plan.rollback_order
            .iter()
            .position(|id| id == &node.id)
            .unwrap()
    };
    assert!(
        compensation("worker", ServiceControlStage::Restore)
            < compensation("database", ServiceControlStage::Restore)
    );
    assert!(
        compensation("database", ServiceControlStage::Quiesce)
            < compensation("worker", ServiceControlStage::Quiesce)
    );
}

#[test]
fn missing_stop_edge_is_not_a_valid_serialized_plan() {
    let mut input = common::sample_input();
    input
        .service_controls
        .push(control("worker", ServiceControlStage::Quiesce));
    let mut plan = compile_transaction(&input).unwrap();
    let stop = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::ServiceControl { .. }))
        .unwrap()
        .id
        .clone();
    let file = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .unwrap()
        .id
        .clone();
    plan.dependencies
        .retain(|edge| edge.from != stop || edge.to != file);
    assert!(plan.validate().is_err());
}

#[test]
fn serialized_service_plans_cannot_bypass_staging_or_durable_intent() {
    let mut input = common::sample_input();
    input
        .service_controls
        .push(control("worker", ServiceControlStage::Quiesce));
    let plan = compile_transaction(&input).unwrap();
    for edge in plan.dependencies.iter().filter(|edge| {
        edge.to.as_str() == OperationId::PREFLIGHT
            || edge.to.as_str() == OperationId::COMMIT_INTENT
            || edge.to.as_str() == OperationId::COMMIT
    }) {
        let mut altered = plan.clone();
        altered.dependencies.retain(|candidate| candidate != edge);
        let serialized = serde_json::to_vec(&altered).unwrap();
        let restored: zup_transaction::TransactionPlan =
            serde_json::from_slice(&serialized).unwrap();
        assert!(
            restored.validate().is_err(),
            "removed safety dependency: {edge:?}"
        );
    }
}

#[test]
fn an_ordered_graph_without_staged_content_cannot_quiesce_a_service() {
    let mut input = common::sample_input();
    input
        .service_controls
        .push(control("worker", ServiceControlStage::Quiesce));
    let mut plan = compile_transaction(&input).unwrap();
    let stages: std::collections::BTreeSet<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::StageFile { .. }))
        .map(|node| node.id.clone())
        .collect();
    assert!(!stages.is_empty());
    plan.nodes.retain(|node| !stages.contains(&node.id));
    for (index, node) in plan.nodes.iter_mut().enumerate() {
        node.declaration_order = index as u32 + 1;
    }
    plan.execution_order.retain(|id| !stages.contains(id));
    plan.rollback_order.retain(|id| !stages.contains(id));
    plan.dependencies
        .retain(|edge| !stages.contains(&edge.from) && !stages.contains(&edge.to));
    plan.dependencies.push(zup_transaction::Dependency {
        from: OperationId::new(OperationId::BEGIN),
        to: OperationId::new(OperationId::PREFLIGHT),
    });
    assert!(plan.validate().is_err());
}

#[test]
fn service_dependency_cycle_is_refused() {
    let mut input = common::sample_input();
    let mut a = control("a", ServiceControlStage::Quiesce);
    let mut b = control("b", ServiceControlStage::Quiesce);
    a.dependencies.push(b.key.clone());
    b.dependencies.push(a.key.clone());
    input.service_controls = vec![a, b];
    assert!(compile_transaction(&input).is_err());
}

#[test]
fn stop_verification_precedes_the_first_file_mutation() {
    let mut input = common::sample_input();
    let mut stop_intent = control("worker", ServiceControlStage::Quiesce);
    stop_intent.privilege = Privilege::User;
    input.service_controls.push(stop_intent);
    let plan = compile_transaction(&input).unwrap();
    let stop = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::ServiceControl { .. }))
        .unwrap()
        .id
        .to_string();
    let files = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    let root = tempfile::tempdir().unwrap();
    let coordinator = zup_transaction::TransactionCoordinator::new(
        zup_transaction::FilesystemTransactionStore::private_at(root.path()),
    );
    let open = coordinator
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            plan,
        )
        .unwrap();
    let mut executor = common::fake::FakeExecutor::new();
    let settled = open.execute(&mut executor).unwrap();
    assert_eq!(
        settled.outcome,
        zup_transaction::TransactionOutcome::Committed
    );
    let verified = executor
        .calls
        .iter()
        .position(|call| call.verb == "verify" && call.id == stop)
        .unwrap();
    for file in files {
        let applied = executor
            .calls
            .iter()
            .position(|call| call.verb == "apply" && call.id == file)
            .unwrap();
        assert!(verified < applied);
    }
}

#[test]
fn modified_files_are_verified_before_any_service_is_started() {
    let mut input = common::sample_input();
    let mut restore = control("worker", ServiceControlStage::Restore);
    restore.privilege = Privilege::User;
    input.service_controls.push(restore);
    let plan = compile_transaction(&input).unwrap();
    let start = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::ServiceControl { .. }))
        .unwrap()
        .id
        .clone();
    let files = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let root = tempfile::tempdir().unwrap();
    let open = zup_transaction::TransactionCoordinator::new(
        zup_transaction::FilesystemTransactionStore::private_at(root.path()),
    )
    .begin(
        common::sample_app_id(),
        zup_core::SelectedScope::User,
        common::sample_version(),
        plan,
    )
    .unwrap();
    let mut executor = common::fake::FakeExecutor::new();
    assert_eq!(
        open.execute(&mut executor).unwrap().outcome,
        zup_transaction::TransactionOutcome::Committed
    );
    let started = executor.position("apply", &start).unwrap();
    for file in files {
        assert!(executor.position("verify", &file).unwrap() < started);
    }
}

#[rstest::rstest]
#[case::new_runtime_cannot_stop(0)]
#[case::restoration_incomplete(1)]
fn failed_compensation_cannot_restore_files_under_a_new_runtime_or_restart_old_runtime(
    #[case] fail_at: usize,
) {
    let mut input = common::sample_input();
    for stage in [ServiceControlStage::Quiesce, ServiceControlStage::Restore] {
        let mut operation = control("worker", stage);
        operation.privilege = Privilege::User;
        input.service_controls.push(operation);
    }
    let plan = compile_transaction(&input).unwrap();
    let stop = plan.nodes.iter().find(|node| matches!(&node.kind, NodeKind::ServiceControl { operation } if operation.stage == ServiceControlStage::Quiesce)).unwrap().id.clone();
    let start = plan.nodes.iter().find(|node| matches!(&node.kind, NodeKind::ServiceControl { operation } if operation.stage == ServiceControlStage::Restore)).unwrap().id.clone();
    let files = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let root = tempfile::tempdir().unwrap();
    let coordinator = zup_transaction::TransactionCoordinator::new(
        zup_transaction::FilesystemTransactionStore::private_at(root.path()),
    );
    let open = coordinator
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            plan,
        )
        .unwrap();
    let mut executor = common::fake::FakeExecutor::new().fail_on_rollback(fail_at);
    executor.fail_verify_operation = Some(start.clone());
    let settled = open.execute(&mut executor).unwrap();
    assert_eq!(
        settled.outcome,
        zup_transaction::TransactionOutcome::RecoveryRequired
    );
    assert!(settled.record.failure.is_some() && settled.record.rollback_failure.is_some());
    assert!(executor.position("rollback", &start).is_some());
    assert!(executor.position("rollback", &stop).is_none());
    if fail_at == 0 {
        for file in files {
            assert!(executor.position("rollback", &file).is_none());
        }
        assert!(
            matches!(settled.record.nodes.get(&start), Some(zup_transaction::NodeState::RollingBack { receipt }) if matches!(receipt.as_ref(), zup_transaction::OperationReceipt::ServiceControl(_)))
        );
    }
    settled.record.validate().unwrap();
}

struct Manager {
    observation: ServiceObservation,
    requests: usize,
    fail_after_request: bool,
    stopped_start_compensatable: bool,
    concurrent_execution: Option<String>,
}

impl NativeServiceManager for Manager {
    fn stopped_start_can_be_compensated(
        &mut self,
        _: &ServiceControlOperation,
    ) -> Result<bool, String> {
        Ok(self.stopped_start_compensatable)
    }
    fn observe(&mut self, _: &ServiceControlOperation) -> Result<ServiceObservation, String> {
        Ok(self.observation.clone())
    }
    fn transition(
        &mut self,
        _: &ServiceControlOperation,
        state: ServiceTerminalState,
        expected: &ServiceObservation,
    ) -> Result<(), String> {
        if let Some(generation) = self.concurrent_execution.take() {
            self.observation.execution_generation = generation;
        }
        if &self.observation != expected {
            return Err("service changed before request".into());
        }
        self.requests += 1;
        self.observation.runtime = match state {
            ServiceTerminalState::Stopped => ServiceRuntimeState::Stopped,
            ServiceTerminalState::Running => ServiceRuntimeState::Running,
        };
        self.observation.execution_generation = match state {
            ServiceTerminalState::Stopped => "stopped",
            ServiceTerminalState::Running => "execution-2",
        }
        .into();
        if self.fail_after_request {
            return Err("reply lost".into());
        }
        Ok(())
    }
}

fn manager(operation: &ServiceControlOperation) -> Manager {
    Manager {
        observation: ServiceObservation {
            configuration: operation.configuration.clone(),
            runtime: ServiceRuntimeState::Running,
            manager_generation: operation.manager_generation.clone(),
            execution_generation: operation.before_execution_generation.clone(),
        },
        requests: 0,
        fail_after_request: false,
        stopped_start_compensatable: false,
        concurrent_execution: None,
    }
}

#[test]
fn a_new_execution_between_authorization_and_dispatch_is_not_stopped() {
    let operation = control("worker", ServiceControlStage::Quiesce);
    let mut manager = manager(&operation);
    manager.concurrent_execution = Some("administrator-restarted".into());
    assert!(apply_service_control(&mut manager, &operation).is_err());
    assert_eq!(manager.requests, 0);
    assert_eq!(manager.observation.runtime, ServiceRuntimeState::Running);
}

#[test]
fn uncertain_stop_is_not_repeated_or_invented_as_a_receipt() {
    let operation = control("worker", ServiceControlStage::Quiesce);
    let mut manager = manager(&operation);
    manager.fail_after_request = true;
    assert!(apply_service_control(&mut manager, &operation).is_err());
    assert_eq!(
        reconcile_service_control(&mut manager, &operation, None).unwrap(),
        ReconcileResult::Ambiguous
    );
    assert_eq!(manager.requests, 1);
}

#[rstest::rstest]
#[case(ServiceControlStage::Quiesce, ServiceRuntimeState::Running)]
#[case(ServiceControlStage::Restore, ServiceRuntimeState::Stopped)]
fn original_runtime_does_not_prove_an_uncertain_request_was_never_submitted(
    #[case] stage: ServiceControlStage,
    #[case] runtime: ServiceRuntimeState,
) {
    let operation = control("worker", stage);
    let mut manager = manager(&operation);
    manager.observation.runtime = runtime;
    assert_eq!(
        reconcile_service_control(&mut manager, &operation, None).unwrap(),
        ReconcileResult::Ambiguous
    );
    assert_eq!(manager.requests, 0);
}

#[test]
fn settled_failed_start_can_only_be_compensated_and_never_verified_as_success() {
    let operation = control("worker", ServiceControlStage::Restore);
    let mut manager = manager(&operation);
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.stopped_start_compensatable = true;
    let ReconcileResult::AppliedWithReceipt(zup_transaction::OperationReceipt::ServiceControl(
        receipt,
    )) = reconcile_service_control(&mut manager, &operation, None).unwrap()
    else {
        panic!("compensation evidence required");
    };
    assert!(verify_service_control(&mut manager, &receipt).is_err());
    rollback_service_control(&mut manager, &receipt).unwrap();
    assert_eq!(manager.requests, 0);
    manager.stopped_start_compensatable = false;
    assert!(rollback_service_control(&mut manager, &receipt).is_err());
    assert_eq!(manager.requests, 0);
}

#[test]
fn recovery_verifies_a_receipted_stop_before_resuming_file_mutation() {
    use zup_transaction::{NodeState, TransactionStore};
    let mut input = common::sample_input();
    let mut operation = control("worker", ServiceControlStage::Quiesce);
    operation.privilege = Privilege::User;
    input.service_controls.push(operation);
    let plan = compile_transaction(&input).unwrap();
    let stop = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::ServiceControl { .. }))
        .unwrap()
        .clone();
    let root = tempfile::tempdir().unwrap();
    let store = zup_transaction::FilesystemTransactionStore::private_at(root.path());
    let mut record = zup_transaction::TransactionRecord::new(
        zup_transaction::TransactionId::new_v7(),
        common::sample_app_id(),
        zup_core::SelectedScope::User,
        common::sample_version(),
        plan,
    );
    record.phase = zup_transaction::TransactionPhase::Applying;
    for id in &record.plan.execution_order {
        if id == &stop.id {
            break;
        }
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| &node.id == id)
            .unwrap();
        record.nodes.insert(
            id.clone(),
            NodeState::Applied {
                receipt: Box::new(common::fake::receipt_for(node)),
            },
        );
    }
    record.nodes.insert(
        stop.id.clone(),
        NodeState::Applied {
            receipt: Box::new(common::fake::receipt_for(&stop)),
        },
    );
    record.validate().unwrap();
    store.create(&record).unwrap();
    let mut executor = common::fake::FakeExecutor::new().fail_on_verify(0);
    let (_, outcome) = zup_transaction::recover(record, &store, &mut executor).unwrap();
    assert_eq!(outcome, zup_transaction::TransactionOutcome::RolledBack);
    assert!(executor.applied.is_empty());
    assert!(executor.position("verify", &stop.id).is_some());
}

#[test]
fn external_execution_change_refuses_control_and_compensation() {
    let operation = control("worker", ServiceControlStage::Quiesce);
    let mut manager = manager(&operation);
    manager.observation.execution_generation = "administrator-restart".into();
    assert!(apply_service_control(&mut manager, &operation).is_err());
    assert_eq!(manager.requests, 0);
    manager.observation.execution_generation = operation.before_execution_generation.clone();
    let zup_transaction::OperationReceipt::ServiceControl(receipt) =
        apply_service_control(&mut manager, &operation).unwrap()
    else {
        panic!("service receipt");
    };
    verify_service_control(&mut manager, &receipt).unwrap();
    manager.observation.manager_generation = "replacement-manager".into();
    assert!(rollback_service_control(&mut manager, &receipt).is_err());
    assert_eq!(manager.requests, 1);
}
