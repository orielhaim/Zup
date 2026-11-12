use serde::{Deserialize, Serialize};
use zup_plan::{
    ServiceControlIntent, ServiceControlOperation, ServiceControlStage, ServiceRuntimeState,
    ServiceState, ServiceTerminalState, disabled_startup,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlReceipt {
    pub operation: ServiceControlOperation,
    pub completion: ServiceControlCompletion,
    pub authority: ServiceControlAuthority,
    /// An authoritative execution generation supplied by the native manager.
    pub execution_generation: String,
    pub process_scope: zup_plan::ServiceScopeObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceControlAuthority {
    Planned,
    ReobservedQuiescent { manager_generation: String },
}

impl ServiceControlReceipt {
    fn authorized_operation(&self) -> ServiceControlOperation {
        let mut operation = self.operation.clone();
        operation.process_scope = self.process_scope.clone();
        if let ServiceControlAuthority::ReobservedQuiescent { manager_generation } = &self.authority
        {
            operation.manager_generation.clone_from(manager_generation);
        }
        operation
    }
}

pub fn prepare_service_compensation(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<ServiceControlReceipt, String> {
    receipt.operation.validate()?;
    let deadline =
        std::time::Instant::now() + receipt.operation.timeout_for(ServiceTerminalState::Stopped);
    manager.prepare_recovery(&receipt.authorized_operation(), deadline)?;
    manager.restore_process_scope(&receipt.authorized_operation(), &receipt.process_scope)?;
    let current = manager.observe(&receipt.operation)?;
    check_deadline(deadline)?;
    let authorized = receipt.authorized_operation();
    if current.manager_generation == authorized.manager_generation {
        return Ok(receipt.clone());
    }
    let mut fresh = receipt.operation.clone();
    fresh
        .manager_generation
        .clone_from(&current.manager_generation);
    let configuration = match fresh.stage {
        ServiceControlStage::Quiesce => &fresh.configuration,
        ServiceControlStage::Restore => &fresh.before_configuration,
    };
    check_identity(&fresh, &current, configuration)?;
    if fresh.intent != ServiceControlIntent::Transition
        || disabled_startup(configuration).as_ref() != Some(configuration)
        || current.runtime != ServiceRuntimeState::Stopped
        || current.execution_generation != "stopped"
        || !manager.stopped_runtime_is_quiescent(&fresh, configuration)?
    {
        return Err(
            "service manager restarted without a verified closed and quiescent activation gate"
                .into(),
        );
    }
    let confirmed = manager.observe(&fresh)?;
    check_deadline(deadline)?;
    if confirmed != current {
        return Err("service authority changed while reobserving compensation".into());
    }
    let mut recovered = receipt.clone();
    recovered.authority = ServiceControlAuthority::ReobservedQuiescent {
        manager_generation: current.manager_generation,
    };
    recovered.execution_generation = current.execution_generation;
    recovered.process_scope = current.process_scope;
    recovered.completion = match fresh.stage {
        ServiceControlStage::Quiesce => ServiceControlCompletion::Reached,
        ServiceControlStage::Restore => ServiceControlCompletion::CompensationOnly,
    };
    crate::OperationReceipt::ServiceControl(Box::new(recovered.clone())).validate()?;
    Ok(recovered)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceControlCompletion {
    Reached,
    CompensationOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceObservation {
    pub registration: zup_core::ObservedServiceRegistration,
    pub configuration: ServiceState,
    pub runtime: ServiceRuntimeState,
    pub manager_generation: String,
    pub execution_generation: String,
    pub process_scope: zup_plan::ServiceScopeObservation,
}

pub trait NativeServiceManager {
    fn prepare_recovery(
        &mut self,
        _operation: &ServiceControlOperation,
        _deadline: std::time::Instant,
    ) -> Result<(), String> {
        Ok(())
    }

    fn restore_process_scope(
        &mut self,
        _operation: &ServiceControlOperation,
        _scope: &zup_plan::ServiceScopeObservation,
    ) -> Result<(), String> {
        Ok(())
    }

    fn reconcile_control_request(
        &mut self,
        _operation: &ServiceControlOperation,
        _deadline: std::time::Instant,
    ) -> Result<(), String> {
        Ok(())
    }
    fn finish_startup_inhibition(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<(), String> {
        if operation.before_configuration != operation.configuration {
            return Err("native startup inhibition is unsupported".into());
        }
        Ok(())
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        _operation: &ServiceControlOperation,
        _configuration: &ServiceState,
    ) -> Result<bool, String> {
        Ok(false)
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        _operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        Ok(false)
    }
    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String>;
    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        state: ServiceTerminalState,
        expected: &ServiceObservation,
        deadline: std::time::Instant,
    ) -> Result<ServiceObservation, String>;
}

fn check_deadline(deadline: std::time::Instant) -> Result<(), String> {
    if std::time::Instant::now() >= deadline {
        Err("service lifecycle observation deadline expired".into())
    } else {
        Ok(())
    }
}

fn check_identity(
    operation: &ServiceControlOperation,
    current: &ServiceObservation,
    configuration: &ServiceState,
) -> Result<(), String> {
    operation.validate()?;
    let zup_core::ResourceKey::Service { id } = &operation.key else {
        return Err("service control requires a logical service resource".into());
    };
    if &current.configuration != configuration
        || current.manager_generation != operation.manager_generation
        || !current.registration.matches(operation.registration, id)
    {
        return Err(format!(
            "service `{}` registration or manager generation changed",
            operation.native_name
        ));
    }
    Ok(())
}

fn closed_activation_matches_before(
    operation: &ServiceControlOperation,
    current: &ServiceObservation,
) -> bool {
    operation.stage == ServiceControlStage::Restore
        && operation.before == ServiceTerminalState::Stopped
        && current.configuration == operation.before_configuration
        && current.manager_generation == operation.manager_generation
        && current.runtime == ServiceRuntimeState::Stopped
        && current.execution_generation == "stopped"
        && matches!(
            current.configuration,
            ServiceState::Registration {
                start: zup_core::ServiceStart::Disabled,
                ..
            }
        )
}

pub fn apply_service_control(
    manager: &mut dyn NativeServiceManager,
    operation: &ServiceControlOperation,
) -> Result<crate::OperationReceipt, String> {
    operation.validate()?;
    let deadline = std::time::Instant::now() + operation.timeout_for(operation.after);
    let current = manager.observe(operation)?;
    check_deadline(deadline)?;
    check_identity(operation, &current, &operation.before_configuration)?;
    if !operation.before.matches(current.runtime)
        || operation.before_execution_generation != current.execution_generation
    {
        return Err(format!(
            "service `{}` runtime changed since authorization",
            operation.native_name
        ));
    }
    crate::fault_injection::boundary(
        crate::fault_injection::Boundary::NativeRequestBefore,
        &operation.native_name,
    );
    let current = if operation.intent == ServiceControlIntent::Transition {
        manager.transition(operation, operation.after, &current, deadline)?
    } else {
        current
    };
    if operation.intent == ServiceControlIntent::Transition
        && operation.stage == ServiceControlStage::Quiesce
    {
        manager.finish_startup_inhibition(operation)?;
    }
    check_deadline(deadline)?;
    check_identity(operation, &current, &operation.configuration)?;
    if !operation.after.matches(current.runtime) {
        return Err(format!(
            "service `{}` did not reach {:?}",
            operation.native_name, operation.after
        ));
    }
    if operation.intent != ServiceControlIntent::Transition
        && current.execution_generation != operation.before_execution_generation
    {
        return Err("service execution changed during runtime preservation".into());
    }
    if operation.after == ServiceTerminalState::Stopped
        && operation.intent == ServiceControlIntent::Transition
        && operation.stage == ServiceControlStage::Quiesce
        && !manager.stopped_runtime_is_quiescent(operation, &operation.configuration)?
    {
        return Err("service stopped but a queued or supervised restart cannot be excluded".into());
    }
    let current = if operation.stage == ServiceControlStage::Quiesce
        && operation.intent == ServiceControlIntent::Transition
    {
        manager.observe(operation)?
    } else {
        current
    };
    let receipt = crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
        operation: operation.clone(),
        completion: ServiceControlCompletion::Reached,
        authority: ServiceControlAuthority::Planned,
        execution_generation: current.execution_generation,
        process_scope: current.process_scope,
    }));
    receipt.validate()?;
    check_deadline(deadline)?;
    Ok(receipt)
}

pub fn verify_service_control(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<(), String> {
    let deadline =
        std::time::Instant::now() + receipt.operation.timeout_for(receipt.operation.after);
    verify_service_control_before(manager, receipt, deadline)
}

fn verify_service_control_before(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
    deadline: std::time::Instant,
) -> Result<(), String> {
    receipt.operation.validate()?;
    check_deadline(deadline)?;
    if receipt.completion != ServiceControlCompletion::Reached {
        return Err("service startup did not complete; compensation is required".into());
    }
    manager.restore_process_scope(&receipt.authorized_operation(), &receipt.process_scope)?;
    let current = manager.observe(&receipt.operation)?;
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.configuration,
    )?;
    if !receipt.operation.after.matches(current.runtime)
        || current.execution_generation != receipt.execution_generation
    {
        return Err(format!(
            "service `{}` execution changed after control",
            receipt.operation.native_name
        ));
    }
    if receipt.operation.intent == ServiceControlIntent::Transition
        && receipt.operation.after == ServiceTerminalState::Stopped
        && receipt.operation.stage == ServiceControlStage::Quiesce
        && !manager
            .stopped_runtime_is_quiescent(&receipt.operation, &receipt.operation.configuration)?
    {
        return Err("service process scope is no longer quiescent".into());
    }
    let confirmed = manager.observe(&receipt.operation)?;
    if confirmed != current {
        return Err(
            "service authority changed during verification; reobserve the native manager".into(),
        );
    }
    check_deadline(deadline)
}

pub fn rollback_service_control(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<(), String> {
    let mut authorized = receipt.clone();
    authorized.operation = receipt.authorized_operation();
    let receipt = &authorized;
    receipt.operation.validate()?;
    let deadline =
        std::time::Instant::now() + receipt.operation.timeout_for(receipt.operation.before);
    if receipt.operation.intent != ServiceControlIntent::Transition {
        return verify_service_control_before(manager, receipt, deadline);
    }
    manager.restore_process_scope(&receipt.authorized_operation(), &receipt.process_scope)?;
    let current = manager.observe(&receipt.operation)?;
    check_deadline(deadline)?;
    if receipt.completion == ServiceControlCompletion::CompensationOnly
        && closed_activation_matches_before(&receipt.operation, &current)
    {
        if !manager.stopped_runtime_is_quiescent(
            &receipt.operation,
            &receipt.operation.before_configuration,
        )? {
            return Err("closed activation does not establish process quiescence".into());
        }
        return check_deadline(deadline);
    }
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.configuration,
    )?;
    if receipt.operation.before == ServiceTerminalState::Stopped
        && current.runtime == ServiceRuntimeState::Stopped
    {
        if receipt.operation.before_configuration == receipt.operation.configuration
            && !manager.stopped_runtime_is_quiescent(
                &receipt.operation,
                &receipt.operation.configuration,
            )?
        {
            return Err("service shutdown still permits a queued or supervised restart".into());
        }
        if receipt.completion == ServiceControlCompletion::CompensationOnly
            && !manager.stopped_start_can_be_compensated(&receipt.operation)?
        {
            return Err("incomplete service startup is no longer safe to compensate".into());
        }
        if receipt.operation.before_configuration == receipt.operation.configuration {
            return check_deadline(deadline);
        }
    } else {
        verify_service_control_before(manager, receipt, deadline)?;
    }
    let mut inverse = receipt.operation.clone();
    std::mem::swap(
        &mut inverse.before_configuration,
        &mut inverse.configuration,
    );
    std::mem::swap(&mut inverse.before, &mut inverse.after);
    inverse.stage = match inverse.stage {
        ServiceControlStage::Quiesce => ServiceControlStage::Restore,
        ServiceControlStage::Restore => ServiceControlStage::Quiesce,
    };
    inverse.before_execution_generation = current.execution_generation.clone();
    inverse.process_scope = current.process_scope.clone();
    if inverse.owned_configuration == ServiceState::Absent {
        inverse.owned_configuration = receipt.operation.configuration.clone();
    }
    check_deadline(deadline)?;
    manager.transition(&inverse, inverse.after, &current, deadline)?;
    if inverse.stage == ServiceControlStage::Quiesce {
        manager.finish_startup_inhibition(&inverse)?;
    }
    manager.restore_process_scope(&receipt.authorized_operation(), &receipt.process_scope)?;
    let current = manager.observe(&receipt.operation)?;
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.before_configuration,
    )?;
    if !receipt.operation.before.matches(current.runtime) {
        return Err(format!(
            "service `{}` previous runtime could not be restored",
            receipt.operation.native_name
        ));
    }
    if inverse.stage == ServiceControlStage::Quiesce
        && inverse.after == ServiceTerminalState::Stopped
        && !manager.stopped_runtime_is_quiescent(&inverse, &inverse.configuration)?
    {
        return Err("service compensation did not establish quiescence".into());
    }
    check_deadline(deadline)
}

pub fn reconcile_service_rollback(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<crate::RollbackReconcileResult, String> {
    let mut authorized = receipt.clone();
    authorized.operation = receipt.authorized_operation();
    let receipt = &authorized;
    receipt.operation.validate()?;
    let deadline =
        std::time::Instant::now() + receipt.operation.timeout_for(receipt.operation.before);
    if receipt.operation.intent != ServiceControlIntent::Transition {
        return Ok(
            if verify_service_control_before(manager, receipt, deadline).is_ok() {
                crate::RollbackReconcileResult::Restored
            } else {
                crate::RollbackReconcileResult::Ambiguous
            },
        );
    }
    manager.restore_process_scope(&receipt.authorized_operation(), &receipt.process_scope)?;
    let current = manager.observe(&receipt.operation)?;
    check_deadline(deadline)?;
    if current.configuration != receipt.operation.configuration {
        let restored = closed_activation_matches_before(&receipt.operation, &current)
            && manager.stopped_runtime_is_quiescent(
                &receipt.operation,
                &receipt.operation.before_configuration,
            )?;
        check_deadline(deadline)?;
        return Ok(if restored {
            crate::RollbackReconcileResult::Restored
        } else {
            crate::RollbackReconcileResult::Ambiguous
        });
    }
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.configuration,
    )?;
    if receipt.operation.before == ServiceTerminalState::Stopped
        && current.runtime == ServiceRuntimeState::Stopped
        && receipt.operation.before_configuration == receipt.operation.configuration
        && manager
            .stopped_runtime_is_quiescent(&receipt.operation, &receipt.operation.configuration)?
    {
        check_deadline(deadline)?;
        return Ok(crate::RollbackReconcileResult::Restored);
    }
    if receipt.completion == ServiceControlCompletion::Reached
        && receipt.operation.before == ServiceTerminalState::Stopped
        && receipt.operation.after == ServiceTerminalState::Running
        && current.runtime == ServiceRuntimeState::Stopped
        && manager
            .stopped_runtime_is_quiescent(&receipt.operation, &receipt.operation.configuration)?
    {
        check_deadline(deadline)?;
        return Ok(crate::RollbackReconcileResult::Retry);
    }
    if receipt.completion == ServiceControlCompletion::CompensationOnly {
        let retry = current.runtime == ServiceRuntimeState::Stopped
            && current.execution_generation == receipt.execution_generation
            && manager.stopped_start_can_be_compensated(&receipt.operation)?;
        check_deadline(deadline)?;
        return Ok(if retry {
            crate::RollbackReconcileResult::Retry
        } else {
            crate::RollbackReconcileResult::Ambiguous
        });
    }
    Ok(
        if verify_service_control_before(manager, receipt, deadline).is_ok() {
            crate::RollbackReconcileResult::Retry
        } else {
            crate::RollbackReconcileResult::Ambiguous
        },
    )
}

pub fn reconcile_service_control(
    manager: &mut dyn NativeServiceManager,
    operation: &ServiceControlOperation,
    receipt: Option<&ServiceControlReceipt>,
) -> Result<crate::ReconcileResult, String> {
    operation.validate()?;
    let deadline = std::time::Instant::now() + operation.timeout_for(operation.after);
    if receipt.is_none() {
        manager.prepare_recovery(operation, deadline)?;
    }
    let current = manager.observe(operation)?;
    check_deadline(deadline)?;
    if current.manager_generation != operation.manager_generation {
        let historical = ServiceControlReceipt {
            operation: operation.clone(),
            completion: match operation.stage {
                ServiceControlStage::Quiesce => ServiceControlCompletion::Reached,
                ServiceControlStage::Restore => ServiceControlCompletion::CompensationOnly,
            },
            authority: ServiceControlAuthority::Planned,
            execution_generation: "stopped".into(),
            process_scope: operation.process_scope.clone(),
        };
        return match prepare_service_compensation(manager, receipt.unwrap_or(&historical)) {
            Ok(recovered) => Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::ServiceControl(Box::new(recovered)),
            )),
            Err(_) => Ok(crate::ReconcileResult::Ambiguous),
        };
    }
    if operation.intent != ServiceControlIntent::Transition && receipt.is_none() {
        let current = manager.observe(operation)?;
        check_deadline(deadline)?;
        check_identity(operation, &current, &operation.configuration)?;
        if operation.after.matches(current.runtime)
            && current.execution_generation == operation.before_execution_generation
        {
            return Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                    operation: operation.clone(),
                    completion: ServiceControlCompletion::Reached,
                    authority: ServiceControlAuthority::Planned,
                    execution_generation: current.execution_generation,
                    process_scope: current.process_scope,
                })),
            ));
        }
        return Ok(crate::ReconcileResult::Ambiguous);
    }
    if let Some(receipt) = receipt {
        if &receipt.operation != operation {
            return Ok(crate::ReconcileResult::Ambiguous);
        }
        return Ok(
            if verify_service_control_before(manager, receipt, deadline).is_ok() {
                crate::ReconcileResult::Applied
            } else {
                crate::ReconcileResult::Ambiguous
            },
        );
    }
    manager.reconcile_control_request(operation, deadline)?;
    let mut current = manager.observe(operation)?;
    check_deadline(deadline)?;
    while matches!(
        current.runtime,
        ServiceRuntimeState::StartPending | ServiceRuntimeState::StopPending
    ) {
        if current.manager_generation != operation.manager_generation
            || current.configuration != operation.before_configuration
                && current.configuration != operation.configuration
        {
            return Err("service authority changed while reconciling a pending request".into());
        }
        check_deadline(deadline)?;
        std::thread::park_timeout(
            std::time::Duration::from_millis(20)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
        current = manager.observe(operation)?;
        check_deadline(deadline)?;
    }
    if operation.after == ServiceTerminalState::Running
        && closed_activation_matches_before(operation, &current)
    {
        if !manager.stopped_runtime_is_quiescent(operation, &operation.before_configuration)? {
            return Ok(crate::ReconcileResult::Ambiguous);
        }
        check_deadline(deadline)?;
        return Ok(crate::ReconcileResult::AppliedWithReceipt(
            crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                operation: operation.clone(),
                completion: ServiceControlCompletion::CompensationOnly,
                authority: ServiceControlAuthority::Planned,
                execution_generation: current.execution_generation,
                process_scope: current.process_scope,
            })),
        ));
    }
    if operation.before_configuration != operation.configuration
        && operation.stage == ServiceControlStage::Quiesce
        && current.configuration == operation.configuration
        && current.manager_generation == operation.manager_generation
        && current.runtime == ServiceRuntimeState::Running
        && current.execution_generation == operation.before_execution_generation
    {
        let mut resume = operation.clone();
        resume.before_configuration = current.configuration.clone();
        check_deadline(deadline)?;
        manager.transition(&resume, ServiceTerminalState::Stopped, &current, deadline)?;
        current = manager.observe(operation)?;
        check_deadline(deadline)?;
    }
    if operation.stage == ServiceControlStage::Quiesce
        && current.configuration == operation.configuration
        && disabled_startup(&current.configuration).as_ref() == Some(&current.configuration)
        && current.manager_generation == operation.manager_generation
        && current.runtime == ServiceRuntimeState::Stopped
    {
        manager.finish_startup_inhibition(operation)?;
        if manager.stopped_runtime_is_quiescent(operation, &operation.configuration)? {
            check_deadline(deadline)?;
            return Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                    operation: operation.clone(),
                    completion: ServiceControlCompletion::Reached,
                    authority: ServiceControlAuthority::Planned,
                    execution_generation: current.execution_generation,
                    process_scope: current.process_scope,
                })),
            ));
        }
    }
    check_identity(operation, &current, &operation.configuration)?;
    if operation.stage == ServiceControlStage::Restore
        && current.runtime == ServiceRuntimeState::Stopped
        && manager.stopped_start_can_be_compensated(operation)?
    {
        check_deadline(deadline)?;
        return Ok(crate::ReconcileResult::AppliedWithReceipt(
            crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                operation: operation.clone(),
                completion: ServiceControlCompletion::CompensationOnly,
                authority: ServiceControlAuthority::Planned,
                execution_generation: current.execution_generation,
                process_scope: current.process_scope,
            })),
        ));
    }
    // A request may still be queued, or a started process may already have exited.
    // Terminal state alone cannot establish that an uncertain request never ran.
    Ok(crate::ReconcileResult::Ambiguous)
}
