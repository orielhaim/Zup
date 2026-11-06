use serde::{Deserialize, Serialize};
use zup_core::{Privilege, ResourceKey};
use zup_plan::{ServiceRuntimeState, ServiceState};

/// A stable terminal state, never a pending manager request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTerminalState {
    Stopped,
    Running,
}

impl ServiceTerminalState {
    pub fn matches(self, observed: ServiceRuntimeState) -> bool {
        matches!(
            (self, observed),
            (Self::Stopped, ServiceRuntimeState::Stopped)
                | (Self::Running, ServiceRuntimeState::Running)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceControlStage {
    Quiesce,
    Restore,
}

/// Authorized runtime intent. Native adapters must verify the complete registration
/// and manager generation, not identify a service by its process id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlOperation {
    pub key: ResourceKey,
    pub native_name: String,
    pub privilege: Privilege,
    pub stage: ServiceControlStage,
    pub before_configuration: ServiceState,
    pub configuration: ServiceState,
    pub owned_configuration: ServiceState,
    pub before: ServiceTerminalState,
    pub after: ServiceTerminalState,
    pub manager_generation: String,
    pub before_execution_generation: String,
    pub timeout_seconds: u32,
    pub rollback_timeout_seconds: u32,
    pub dependencies: Vec<ResourceKey>,
}

impl ServiceControlOperation {
    pub fn validate(&self) -> Result<(), String> {
        for state in [
            &self.before_configuration,
            &self.configuration,
            &self.owned_configuration,
        ] {
            if let ServiceState::Registration {
                execution,
                command,
                failure,
                readiness,
                ..
            } = state
            {
                execution
                    .validate_policies(*failure, *readiness, *command.executable.target())
                    .map_err(str::to_owned)?;
            }
        }
        if !matches!(self.key, ResourceKey::Service { .. })
            || !matches!(self.configuration, ServiceState::Registration { .. })
            || self.native_name.is_empty()
            || self.native_name.len() > 256
            || self.native_name.chars().any(char::is_control)
            || self.manager_generation.is_empty()
            || self.manager_generation.len() > 256
            || self.manager_generation.chars().any(char::is_control)
            || self.before_execution_generation.is_empty()
            || self.before_execution_generation.len() > 256
            || self
                .before_execution_generation
                .chars()
                .any(char::is_control)
            || !(1..=3600).contains(&self.timeout_seconds)
            || !(1..=3600).contains(&self.rollback_timeout_seconds)
            || self.dependencies.len() > crate::MAX_BACKEND_DEPENDENCIES
        {
            return Err(
                "invalid service control identity, registration, generation or deadline".into(),
            );
        }
        if self.owned_configuration == ServiceState::Absent
            && self.stage != ServiceControlStage::Restore
        {
            return Err("an unregistered service cannot be quiesced".into());
        }
        let configuration_transition = self.before_configuration != self.configuration;
        if self.stage == ServiceControlStage::Quiesce
            && requires_startup_inhibition(&self.configuration)
        {
            return Err(
                "SCM recovery actions require disabled startup before service shutdown".into(),
            );
        }
        if configuration_transition {
            let (enabled, disabled) = match self.stage {
                ServiceControlStage::Quiesce => (&self.before_configuration, &self.configuration),
                ServiceControlStage::Restore => (&self.configuration, &self.before_configuration),
            };
            if disabled_startup(enabled).as_ref() != Some(disabled)
                || !matches!(
                    enabled,
                    ServiceState::Registration {
                        execution: zup_core::ServiceExecution::Windows { .. },
                        ..
                    }
                )
                || self.privilege != Privilege::System
            {
                return Err(
                    "service control can only inhibit or restore native Windows startup".into(),
                );
            }
        }
        if (self.before == self.after && !configuration_transition)
            || (self.stage == ServiceControlStage::Quiesce
                && self.after != ServiceTerminalState::Stopped)
            || (self.stage == ServiceControlStage::Restore
                && self.after != ServiceTerminalState::Running
                && !configuration_transition)
        {
            return Err("service control does not describe a lifecycle transition".into());
        }
        if self.after == ServiceTerminalState::Running
            && matches!(
                self.configuration,
                ServiceState::Registration {
                    start: zup_core::ServiceStart::Disabled,
                    ..
                }
            )
        {
            return Err("disabled service cannot be started".into());
        }
        let mut dependencies = std::collections::BTreeSet::new();
        if self.dependencies.iter().any(|key| {
            !matches!(key, ResourceKey::Service { .. })
                || key == &self.key
                || !dependencies.insert(key)
        }) {
            return Err("invalid service dependency".into());
        }
        Ok(())
    }

    pub fn timeout_for(&self, state: ServiceTerminalState) -> std::time::Duration {
        std::time::Duration::from_secs(if state == self.after {
            self.timeout_seconds.into()
        } else {
            self.rollback_timeout_seconds.into()
        })
    }
}

pub fn disabled_startup(state: &ServiceState) -> Option<ServiceState> {
    let mut disabled = state.clone();
    let ServiceState::Registration { start, .. } = &mut disabled else {
        return None;
    };
    *start = zup_core::ServiceStart::Disabled;
    Some(disabled)
}

pub fn requires_startup_inhibition(state: &ServiceState) -> bool {
    matches!(state, ServiceState::Registration { failure: zup_core::ServiceFailurePolicy::Windows { .. }, start, .. } if *start != zup_core::ServiceStart::Disabled)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlReceipt {
    pub operation: ServiceControlOperation,
    pub completion: ServiceControlCompletion,
    /// An authoritative execution generation supplied by the native manager.
    pub execution_generation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceControlCompletion {
    Reached,
    CompensationOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceObservation {
    pub configuration: ServiceState,
    pub runtime: ServiceRuntimeState,
    pub manager_generation: String,
    pub execution_generation: String,
}

pub fn plan_service_controls(
    service: &zup_plan::ServiceOperation,
    owned: Option<&zup_plan::OwnedResource>,
    payload_changed: bool,
    native_name: String,
    manager_generation: String,
    execution_generation: String,
) -> Result<Vec<ServiceControlOperation>, String> {
    service.lifecycle.validate(service.start)?;
    use zup_plan::ObservedServiceState;
    let installed = service.desired_configuration();
    let previous = service
        .previous
        .configuration()
        .ok_or("unrecognized service registration")?;
    if matches!(
        service.previous,
        ObservedServiceState::Service {
            runtime_state: Some(ServiceRuntimeState::Stopped),
            ..
        }
    ) && !requires_startup_inhibition(&previous)
        && !requires_startup_inhibition(&installed)
    {
        return Ok(Vec::new());
    }
    if owned.is_some() && previous == ServiceState::Absent {
        return Ok(Vec::new());
    }
    let owned_configuration = match owned {
        Some(zup_plan::OwnedResource::Service { installed, .. })
            if installed.as_ref() == &previous =>
        {
            installed.as_ref().clone()
        }
        None if previous == ServiceState::Absent => ServiceState::Absent,
        None => return Ok(Vec::new()),
        _ => return Err("service ownership or configuration changed".into()),
    };
    if !payload_changed && previous == installed {
        return Ok(Vec::new());
    }
    let mut operations = Vec::new();
    let mut control =
        |stage, before_configuration, configuration, before, after, before_execution_generation| {
            operations.push(ServiceControlOperation {
                key: service.key.clone(),
                native_name: native_name.clone(),
                privilege: service.privilege,
                stage,
                before_configuration,
                configuration,
                owned_configuration: owned_configuration.clone(),
                before,
                after,
                manager_generation: manager_generation.clone(),
                before_execution_generation,
                timeout_seconds: match stage {
                    ServiceControlStage::Quiesce => service.lifecycle.stop_timeout_seconds,
                    ServiceControlStage::Restore => service.lifecycle.start_timeout_seconds,
                },
                rollback_timeout_seconds: match stage {
                    ServiceControlStage::Quiesce => service.lifecycle.start_timeout_seconds,
                    ServiceControlStage::Restore => service.lifecycle.stop_timeout_seconds,
                },
                dependencies: Vec::new(),
            });
        };
    let running = match &service.previous {
        ObservedServiceState::Absent => false,
        ObservedServiceState::Service {
            runtime_state: Some(ServiceRuntimeState::Running),
            ..
        } => true,
        ObservedServiceState::Service {
            runtime_state: Some(ServiceRuntimeState::Stopped),
            ..
        } => false,
        _ => {
            return Err(
                "affected service must have an authoritative running or stopped observation".into(),
            );
        }
    };
    let inhibited =
        requires_startup_inhibition(&previous) || requires_startup_inhibition(&installed);
    if running
        || inhibited
            && previous != ServiceState::Absent
            && disabled_startup(&previous).as_ref() != Some(&previous)
    {
        control(
            ServiceControlStage::Quiesce,
            previous.clone(),
            if inhibited {
                disabled_startup(&previous).expect("registration")
            } else {
                previous.clone()
            },
            if running {
                ServiceTerminalState::Running
            } else {
                ServiceTerminalState::Stopped
            },
            ServiceTerminalState::Stopped,
            execution_generation,
        );
    }
    let restore = if previous == ServiceState::Absent {
        service.lifecycle.install == zup_core::InitialServiceState::Running
    } else {
        running && service.lifecycle.change == zup_core::ChangedServiceState::Preserve
    };
    if (restore && service.start != zup_core::ServiceStart::Disabled)
        || inhibited && service.start != zup_core::ServiceStart::Disabled
    {
        control(
            ServiceControlStage::Restore,
            if inhibited {
                disabled_startup(&installed).expect("registration")
            } else {
                installed.clone()
            },
            installed,
            ServiceTerminalState::Stopped,
            if restore {
                ServiceTerminalState::Running
            } else {
                ServiceTerminalState::Stopped
            },
            "stopped".into(),
        );
    }
    for operation in &operations {
        operation.validate()?;
    }
    Ok(operations)
}

pub trait NativeServiceManager {
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
    ) -> Result<(), String>;
}

fn check_identity(
    operation: &ServiceControlOperation,
    current: &ServiceObservation,
    configuration: &ServiceState,
) -> Result<(), String> {
    operation.validate()?;
    if &current.configuration != configuration
        || current.manager_generation != operation.manager_generation
    {
        return Err(format!(
            "service `{}` registration or manager generation changed",
            operation.native_name
        ));
    }
    Ok(())
}

pub fn apply_service_control(
    manager: &mut dyn NativeServiceManager,
    operation: &ServiceControlOperation,
) -> Result<crate::OperationReceipt, String> {
    let current = manager.observe(operation)?;
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
    manager.transition(operation, operation.after, &current)?;
    if operation.stage == ServiceControlStage::Quiesce {
        manager.finish_startup_inhibition(operation)?;
    }
    let current = manager.observe(operation)?;
    check_identity(operation, &current, &operation.configuration)?;
    if !operation.after.matches(current.runtime) {
        return Err(format!(
            "service `{}` did not reach {:?}",
            operation.native_name, operation.after
        ));
    }
    if operation.after == ServiceTerminalState::Stopped
        && operation.stage == ServiceControlStage::Quiesce
        && !manager.stopped_runtime_is_quiescent(operation)?
    {
        return Err("service stopped but a queued or supervised restart cannot be excluded".into());
    }
    let receipt = crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
        operation: operation.clone(),
        completion: ServiceControlCompletion::Reached,
        execution_generation: current.execution_generation,
    }));
    receipt.validate()?;
    Ok(receipt)
}

pub fn verify_service_control(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<(), String> {
    if receipt.completion != ServiceControlCompletion::Reached {
        return Err("service startup did not complete; compensation is required".into());
    }
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
    Ok(())
}

pub fn rollback_service_control(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<(), String> {
    let current = manager.observe(&receipt.operation)?;
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.configuration,
    )?;
    if receipt.operation.before == ServiceTerminalState::Stopped
        && current.runtime == ServiceRuntimeState::Stopped
    {
        if receipt.operation.before_configuration == receipt.operation.configuration
            && !manager.stopped_runtime_is_quiescent(&receipt.operation)?
        {
            return Err("service shutdown still permits a queued or supervised restart".into());
        }
        if receipt.completion == ServiceControlCompletion::CompensationOnly
            && !manager.stopped_start_can_be_compensated(&receipt.operation)?
        {
            return Err("incomplete service startup is no longer safe to compensate".into());
        }
        if receipt.operation.before_configuration == receipt.operation.configuration {
            return Ok(());
        }
    } else {
        verify_service_control(manager, receipt)?;
    }
    let mut inverse = receipt.operation.clone();
    std::mem::swap(
        &mut inverse.before_configuration,
        &mut inverse.configuration,
    );
    std::mem::swap(&mut inverse.before, &mut inverse.after);
    std::mem::swap(
        &mut inverse.timeout_seconds,
        &mut inverse.rollback_timeout_seconds,
    );
    inverse.stage = match inverse.stage {
        ServiceControlStage::Quiesce => ServiceControlStage::Restore,
        ServiceControlStage::Restore => ServiceControlStage::Quiesce,
    };
    inverse.before_execution_generation = current.execution_generation.clone();
    if inverse.owned_configuration == ServiceState::Absent {
        inverse.owned_configuration = receipt.operation.configuration.clone();
    }
    manager.transition(&inverse, inverse.after, &current)?;
    if inverse.stage == ServiceControlStage::Quiesce {
        manager.finish_startup_inhibition(&inverse)?;
    }
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
        && !manager.stopped_runtime_is_quiescent(&inverse)?
    {
        return Err("service compensation did not establish quiescence".into());
    }
    Ok(())
}

pub fn reconcile_service_rollback(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<crate::RollbackReconcileResult, String> {
    let current = manager.observe(&receipt.operation)?;
    if current.configuration != receipt.operation.configuration {
        if receipt.operation.before == ServiceTerminalState::Stopped
            && current.configuration == receipt.operation.before_configuration
            && current.manager_generation == receipt.operation.manager_generation
            && current.runtime == ServiceRuntimeState::Stopped
            && matches!(
                &current.configuration,
                ServiceState::Registration {
                    start: zup_core::ServiceStart::Disabled,
                    ..
                }
            )
        {
            return Ok(crate::RollbackReconcileResult::Restored);
        }
        return Ok(crate::RollbackReconcileResult::Ambiguous);
    }
    check_identity(
        &receipt.operation,
        &current,
        &receipt.operation.configuration,
    )?;
    if receipt.operation.before == ServiceTerminalState::Stopped
        && current.runtime == ServiceRuntimeState::Stopped
        && receipt.operation.before_configuration == receipt.operation.configuration
        && manager.stopped_runtime_is_quiescent(&receipt.operation)?
    {
        return Ok(crate::RollbackReconcileResult::Restored);
    }
    Ok(if verify_service_control(manager, receipt).is_ok() {
        crate::RollbackReconcileResult::Retry
    } else {
        crate::RollbackReconcileResult::Ambiguous
    })
}

pub fn reconcile_service_control(
    manager: &mut dyn NativeServiceManager,
    operation: &ServiceControlOperation,
    receipt: Option<&ServiceControlReceipt>,
) -> Result<crate::ReconcileResult, String> {
    if let Some(receipt) = receipt {
        if &receipt.operation != operation {
            return Ok(crate::ReconcileResult::Ambiguous);
        }
        return Ok(if verify_service_control(manager, receipt).is_ok() {
            crate::ReconcileResult::Applied
        } else {
            crate::ReconcileResult::Ambiguous
        });
    }
    let mut current = manager.observe(operation)?;
    if operation.before_configuration != operation.configuration
        && current.configuration == operation.before_configuration
        && current.manager_generation == operation.manager_generation
        && operation.before.matches(current.runtime)
        && current.execution_generation == operation.before_execution_generation
    {
        return Ok(crate::ReconcileResult::NotApplied);
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
        manager.transition(&resume, ServiceTerminalState::Stopped, &current)?;
        current = manager.observe(operation)?;
    }
    if operation.before_configuration != operation.configuration
        && operation.stage == ServiceControlStage::Quiesce
        && current.configuration == operation.configuration
        && current.manager_generation == operation.manager_generation
        && current.runtime == ServiceRuntimeState::Stopped
    {
        manager.finish_startup_inhibition(operation)?;
        if manager.stopped_runtime_is_quiescent(operation)? {
            return Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                    operation: operation.clone(),
                    completion: ServiceControlCompletion::Reached,
                    execution_generation: current.execution_generation,
                })),
            ));
        }
    }
    check_identity(operation, &current, &operation.configuration)?;
    if operation.stage == ServiceControlStage::Restore
        && current.runtime == ServiceRuntimeState::Stopped
        && manager.stopped_start_can_be_compensated(operation)?
    {
        return Ok(crate::ReconcileResult::AppliedWithReceipt(
            crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
                operation: operation.clone(),
                completion: ServiceControlCompletion::CompensationOnly,
                execution_generation: current.execution_generation,
            })),
        ));
    }
    // A request may still be queued, or a started process may already have exited.
    // Terminal state alone cannot establish that an uncertain request never ran.
    Ok(crate::ReconcileResult::Ambiguous)
}

pub fn bind_service_dependencies(
    controls: &mut [ServiceControlOperation],
    execution: &zup_plan::ExecutionPlan,
    ledger: Option<&zup_plan::InstallLedger>,
) -> Result<(), String> {
    let previous = execution.service_graph(ledger, true);
    let desired = execution.service_graph(ledger, false);
    zup_core::validate_service_graph(&previous)?;
    zup_core::validate_service_graph(&desired)?;
    let planned: std::collections::BTreeSet<_> = controls
        .iter()
        .map(|control| (control.stage, control.key.clone()))
        .collect();
    let starts: std::collections::BTreeSet<_> = controls
        .iter()
        .filter(|control| {
            control.stage == ServiceControlStage::Restore
                && control.after == ServiceTerminalState::Running
        })
        .map(|control| control.key.clone())
        .collect();
    for control in controls {
        let graph = match control.stage {
            ServiceControlStage::Quiesce => &previous,
            ServiceControlStage::Restore => &desired,
        };
        let dependencies = graph.get(&control.key).into_iter().flatten();
        for dependency in dependencies.clone() {
            if control.stage == ServiceControlStage::Restore
                && control.after == ServiceTerminalState::Running
                && !starts.contains(dependency)
                && (planned.contains(&(ServiceControlStage::Quiesce, dependency.clone()))
                    || !execution.services.iter().any(|service| {
                        service.key == *dependency
                            && matches!(
                                service.previous,
                                zup_plan::ObservedServiceState::Service {
                                    runtime_state: Some(zup_plan::ServiceRuntimeState::Running),
                                    ..
                                }
                            )
                    }))
            {
                return Err(format!(
                    "required service {dependency:?} is not running and has no authorized startup"
                ));
            }
        }
        control.dependencies = dependencies
            .filter(|key| planned.contains(&(control.stage, (*key).clone())))
            .cloned()
            .collect();
        control.validate()?;
    }
    Ok(())
}
