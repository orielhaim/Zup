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
        native_timeouts: zup_core::NativeServiceTimeouts::Scm,
        dependencies: Vec::new(),
        failure: Default::default(),
        execution: zup_core::ServiceExecution::Windows {
            account: zup_core::WindowsServiceAccount::LocalService,
        },
        readiness: Default::default(),
        display_name: id.into(),
        command: Box::new(CommandSpec::new(
            common::tpath(r"C:\PF\Acme\a.exe"),
            Vec::new(),
        )),
        start: ServiceStart::Manual,
    };
    ServiceControlOperation {
        intent: zup_transaction::ServiceControlIntent::Transition,
        key: ResourceKey::Service {
            id: ServiceId::new(id).unwrap(),
        },
        native_name: id.into(),
        privilege: Privilege::System,
        before_configuration: configuration.clone(),
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
        timeouts: zup_core::ServiceTimeouts::Windows {
            start_observation_seconds: 45,
            stop_observation_seconds: 30,
        },
        dependencies: Vec::new(),
    }
}

#[rstest::rstest]
#[case::payload_replacement(true, zup_transaction::ServiceControlIntent::Transition)]
#[case::observation_policy_only(false, zup_transaction::ServiceControlIntent::Preserve)]
fn lifecycle_changes_bind_observations_and_their_respective_deadlines(
    #[case] payload_changed: bool,
    #[case] expected_intent: zup_transaction::ServiceControlIntent,
) {
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
        failure: Default::default(),
        execution: zup_core::ServiceExecution::Windows {
            account: zup_core::WindowsServiceAccount::LocalService,
        },
        requires_files: Vec::new(),
        requires_services: Vec::new(),
        dependency_names: Vec::new(),
        readiness,
        key: intent.key.clone(),
        kind: zup_plan::IntegrationChangeKind::UpdateOwned,
        id: "worker".into(),
        name: "worker".into(),
        display_name: display_name.clone(),
        command: *command.clone(),
        start,
        lifecycle: zup_core::ServiceLifecycle {
            timeouts: Some(zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 17,
                stop_observation_seconds: 23,
            }),
            ..Default::default()
        },
        privilege: Privilege::System,
        previous: zup_plan::ObservedServiceState::Service {
            native_timeouts: zup_core::NativeServiceTimeouts::Scm,
            dependencies: Vec::new(),
            failure: Default::default(),
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
        timeouts: zup_core::ServiceTimeouts::Windows {
            start_observation_seconds: 90,
            stop_observation_seconds: 90,
        },
        requires_files: Vec::new(),
        requires_services: Vec::new(),
        name: "worker".into(),
        privilege: Privilege::System,
        previous: ServiceState::Absent,
        installed: Box::new(intent.configuration),
    };
    let operations = zup_transaction::plan_service_controls(
        &service,
        Some(&owned),
        payload_changed,
        "worker".into(),
        "boot:manager".into(),
        "execution-1".into(),
    )
    .unwrap();
    assert_eq!(operations.len(), 2);
    for operation in operations {
        assert_eq!(operation.intent, expected_intent);
        let (start, stop) = match operation.stage {
            ServiceControlStage::Quiesce => (90, 90),
            ServiceControlStage::Restore => (17, 23),
        };
        assert_eq!(
            operation
                .timeout_for(ServiceTerminalState::Running)
                .as_secs(),
            start
        );
        assert_eq!(
            operation
                .timeout_for(ServiceTerminalState::Stopped)
                .as_secs(),
            stop
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
    observation_delay: std::time::Duration,
    observation: ServiceObservation,
    requests: usize,
    fail_after_request: bool,
    exit_after_transition: bool,
    stopped_start_compensatable: bool,
    quiescent: bool,
    concurrent_execution: Option<String>,
}

impl NativeServiceManager for Manager {
    fn finish_startup_inhibition(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<(), String> {
        if operation.before_configuration == operation.configuration {
            return Ok(());
        }
        if self.observation.configuration == operation.configuration
            && self.stopped_runtime_is_quiescent(operation, &operation.configuration)?
        {
            Ok(())
        } else {
            Err("startup inhibition is not quiescent".into())
        }
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        _: &ServiceControlOperation,
        _: &ServiceState,
    ) -> Result<bool, String> {
        Ok(self.quiescent && self.observation.runtime == ServiceRuntimeState::Stopped)
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        _: &ServiceControlOperation,
    ) -> Result<bool, String> {
        Ok(self.stopped_start_compensatable)
    }
    fn observe(&mut self, _: &ServiceControlOperation) -> Result<ServiceObservation, String> {
        std::thread::sleep(std::mem::take(&mut self.observation_delay));
        Ok(self.observation.clone())
    }
    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        state: ServiceTerminalState,
        expected: &ServiceObservation,
        _deadline: std::time::Instant,
    ) -> Result<ServiceObservation, String> {
        if let Some(generation) = self.concurrent_execution.take() {
            self.observation.execution_generation = generation;
        }
        if &self.observation != expected {
            return Err("service changed before request".into());
        }
        self.requests += 1;
        self.observation.configuration = operation.configuration.clone();
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
        let completed = self.observation.clone();
        if self.exit_after_transition {
            self.observation.runtime = ServiceRuntimeState::Stopped;
            self.observation.execution_generation = "stopped".into();
        }
        Ok(completed)
    }
}

fn manager(operation: &ServiceControlOperation) -> Manager {
    Manager {
        observation_delay: std::time::Duration::ZERO,
        observation: ServiceObservation {
            configuration: operation.configuration.clone(),
            runtime: ServiceRuntimeState::Running,
            manager_generation: operation.manager_generation.clone(),
            execution_generation: operation.before_execution_generation.clone(),
        },
        requests: 0,
        fail_after_request: false,
        exit_after_transition: false,
        stopped_start_compensatable: false,
        quiescent: true,
        concurrent_execution: None,
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn a_start_that_exits_after_native_completion_keeps_its_execution_receipt(#[case] inhibited: bool) {
    let mut operation = control("worker", ServiceControlStage::Restore);
    if inhibited {
        operation.before_configuration =
            zup_transaction::disabled_startup(&operation.configuration).unwrap();
    }
    let mut manager = manager(&operation);
    manager.observation.configuration = operation.before_configuration.clone();
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.exit_after_transition = true;
    let zup_transaction::OperationReceipt::ServiceControl(receipt) =
        apply_service_control(&mut manager, &operation).unwrap()
    else {
        panic!("service execution receipt required");
    };
    assert_eq!(receipt.execution_generation, "execution-2");
    assert!(verify_service_control(&mut manager, &receipt).is_err());
    manager.quiescent = false;
    assert_eq!(
        zup_transaction::reconcile_service_rollback(&mut manager, &receipt).unwrap(),
        zup_transaction::RollbackReconcileResult::Ambiguous
    );
    manager.quiescent = true;
    assert_eq!(
        zup_transaction::reconcile_service_rollback(&mut manager, &receipt).unwrap(),
        if inhibited {
            zup_transaction::RollbackReconcileResult::Retry
        } else {
            zup_transaction::RollbackReconcileResult::Restored
        }
    );
    rollback_service_control(&mut manager, &receipt).unwrap();
    assert_eq!(manager.observation.runtime, ServiceRuntimeState::Stopped);
    assert_eq!(manager.requests, if inhibited { 2 } else { 1 });
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn expired_observation_cannot_authorize_start_or_recovery(#[case] recovering: bool) {
    let mut operation = control("worker", ServiceControlStage::Restore);
    operation.timeouts = zup_core::ServiceTimeouts::Windows {
        start_observation_seconds: 1,
        stop_observation_seconds: 1,
    };
    let mut manager = manager(&operation);
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.observation_delay = std::time::Duration::from_millis(1050);
    if recovering {
        manager.stopped_start_compensatable = true;
        assert!(reconcile_service_control(&mut manager, &operation, None).is_err());
    } else {
        assert!(apply_service_control(&mut manager, &operation).is_err());
    }
    assert_eq!(manager.requests, 0);
}

fn preserved_policy_update() -> [ServiceControlOperation; 2] {
    let mut before = control("worker", ServiceControlStage::Quiesce);
    before.intent = zup_transaction::ServiceControlIntent::Preserve;
    before.after = before.before;
    let mut after = before.clone();
    after.stage = ServiceControlStage::Restore;
    let ServiceState::Registration { start, .. } = &mut after.configuration else {
        unreachable!()
    };
    *start = ServiceStart::Automatic;
    after.before_configuration = after.configuration.clone();
    [before, after]
}

#[test]
fn an_inhibited_uncertain_start_can_only_be_compensated() {
    let mut operation = control("worker", ServiceControlStage::Restore);
    operation.before_configuration =
        zup_transaction::disabled_startup(&operation.configuration).unwrap();
    let mut manager = manager(&operation);
    manager.observation.configuration = operation.before_configuration.clone();
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.observation.execution_generation = "stopped".into();
    let ReconcileResult::AppliedWithReceipt(zup_transaction::OperationReceipt::ServiceControl(
        receipt,
    )) = reconcile_service_control(&mut manager, &operation, None).unwrap()
    else {
        panic!("closed activation must permit compensation");
    };
    assert!(verify_service_control(&mut manager, &receipt).is_err());
    rollback_service_control(&mut manager, &receipt).unwrap();
    assert_eq!(manager.requests, 0);
}

#[test]
fn policy_preservation_never_controls_runtime_and_rejects_external_restarts() {
    let [before, after] = preserved_policy_update();
    let mut manager = manager(&before);
    let receipt = apply_service_control(&mut manager, &before).unwrap();
    let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
        unreachable!()
    };
    rollback_service_control(&mut manager, &receipt).unwrap();
    manager.observation.configuration = after.configuration.clone();
    apply_service_control(&mut manager, &after).unwrap();
    manager.observation.execution_generation = "external-restart".into();
    assert!(apply_service_control(&mut manager, &after).is_err());
    assert_eq!(manager.requests, 0);
}

#[test]
fn preservation_cannot_authorize_replacing_a_running_service_image() {
    let controls = preserved_policy_update();
    let mut input = zup_transaction::TransactionInput::new(common::target());
    input.service_controls = controls.into();
    compile_transaction(&input).unwrap();
    input.files = common::sample_input().files;
    let executable = input.files[0].destination.clone();
    for operation in &mut input.service_controls {
        for state in [
            &mut operation.before_configuration,
            &mut operation.configuration,
            &mut operation.owned_configuration,
        ] {
            let ServiceState::Registration { command, .. } = state else {
                unreachable!()
            };
            command.executable = executable.clone();
        }
    }
    assert!(compile_transaction(&input).is_err());
}

#[test]
fn a_stopped_service_with_a_possible_queued_restart_does_not_release_compensation() {
    let operation = control("worker", ServiceControlStage::Restore);
    let receipt = zup_transaction::ServiceControlReceipt {
        operation: operation.clone(),
        completion: zup_transaction::ServiceControlCompletion::Reached,
        execution_generation: "execution-2".into(),
    };
    let mut manager = manager(&operation);
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.observation.execution_generation = "stopped".into();
    manager.quiescent = false;
    assert!(rollback_service_control(&mut manager, &receipt).is_err());
    assert_eq!(
        zup_transaction::reconcile_service_rollback(&mut manager, &receipt).unwrap(),
        zup_transaction::RollbackReconcileResult::Ambiguous
    );
    assert_eq!(manager.requests, 0);
    manager.quiescent = true;
    assert_eq!(
        zup_transaction::reconcile_service_rollback(&mut manager, &receipt).unwrap(),
        zup_transaction::RollbackReconcileResult::Restored
    );
    rollback_service_control(&mut manager, &receipt).unwrap();
    assert_eq!(manager.requests, 0);
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

#[rstest::rstest]
#[case(ServiceStart::Automatic)]
#[case(ServiceStart::Manual)]
#[case(ServiceStart::Disabled)]
fn owned_executable_mutation_requires_quiescence_even_without_registration_nodes(
    #[case] start: ServiceStart,
) {
    let mut operation = control("worker", ServiceControlStage::Quiesce);
    let ServiceState::Registration { start: policy, .. } = &mut operation.before_configuration
    else {
        unreachable!()
    };
    *policy = start;
    operation.owned_configuration = operation.before_configuration.clone();
    operation.configuration =
        zup_transaction::disabled_startup(&operation.before_configuration).unwrap();
    if start == ServiceStart::Disabled {
        operation.before = ServiceTerminalState::Stopped;
        operation.before_execution_generation = "stopped".into();
    }
    let mut ledger = zup_plan::InstallLedger::new(
        common::sample_app_id(),
        common::target(),
        zup_core::SelectedScope::Machine,
        zup_core::TransactionId::new_v7(),
    );
    ledger.resources.insert(
        operation.key.clone(),
        zup_plan::OwnedResource::Service {
            timeouts: operation.timeouts,
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            name: operation.native_name.clone(),
            privilege: Privilege::System,
            previous: ServiceState::Absent,
            installed: Box::new(operation.owned_configuration.clone()),
        },
    );
    let mut input = common::sample_input();
    input.backend_operations.clear();
    let unprotected = compile_transaction(&input).unwrap();
    assert!(zup_transaction::validate_owned_service_inputs(&unprotected, Some(&ledger)).is_err());
    input.service_controls.push(operation);
    let protected = compile_transaction(&input).unwrap();
    zup_transaction::validate_owned_service_inputs(&protected, Some(&ledger)).unwrap();
}

#[rstest::rstest]
#[case::empty_scope(true)]
#[case::unproven_scope(false)]
fn lost_quiescence_reply_uses_closed_activation_and_scope_evidence(#[case] quiescent: bool) {
    let mut operation = control("worker", ServiceControlStage::Quiesce);
    let disabled = zup_transaction::disabled_startup(&operation.configuration).unwrap();
    operation.before_configuration = disabled.clone();
    operation.configuration = disabled.clone();
    operation.owned_configuration = disabled;
    operation.before = ServiceTerminalState::Stopped;
    operation.before_execution_generation = "stopped".into();
    let mut manager = manager(&operation);
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.quiescent = quiescent;
    let result = reconcile_service_control(&mut manager, &operation, None).unwrap();
    if quiescent {
        assert!(matches!(result, ReconcileResult::AppliedWithReceipt(_)));
    } else {
        assert_eq!(result, ReconcileResult::Ambiguous);
    }
    assert_eq!(manager.requests, 0);
}

#[rstest::rstest]
#[case::retry_incomplete_start(false, true, zup_transaction::RollbackReconcileResult::Retry)]
#[case::refuse_changed_evidence(false, false, zup_transaction::RollbackReconcileResult::Ambiguous)]
#[case::closed_and_quiescent(true, true, zup_transaction::RollbackReconcileResult::Restored)]
#[case::closed_with_live_scope(true, false, zup_transaction::RollbackReconcileResult::Ambiguous)]
fn interrupted_start_compensation_requires_current_scope_evidence(
    #[case] activation_restored: bool,
    #[case] safe: bool,
    #[case] expected: zup_transaction::RollbackReconcileResult,
) {
    let mut operation = control("worker", ServiceControlStage::Restore);
    operation.before_configuration =
        zup_transaction::disabled_startup(&operation.configuration).unwrap();
    let mut manager = manager(&operation);
    manager.observation.runtime = ServiceRuntimeState::Stopped;
    manager.observation.execution_generation = "stopped".into();
    if activation_restored {
        manager.observation.configuration = operation.before_configuration.clone();
    }
    manager.quiescent = safe;
    manager.stopped_start_compensatable = safe;
    let receipt = zup_transaction::ServiceControlReceipt {
        operation,
        completion: zup_transaction::ServiceControlCompletion::CompensationOnly,
        execution_generation: "stopped".into(),
    };
    assert_eq!(
        zup_transaction::reconcile_service_rollback(&mut manager, &receipt).unwrap(),
        expected
    );
    assert_eq!(manager.requests, 0);
}
