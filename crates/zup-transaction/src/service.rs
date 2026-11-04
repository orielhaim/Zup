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
        if self.before == self.after
            || (self.stage == ServiceControlStage::Quiesce
                && self.after != ServiceTerminalState::Stopped)
            || (self.stage == ServiceControlStage::Restore
                && self.after != ServiceTerminalState::Running)
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
    use zup_plan::{IntegrationChangeKind, ObservedServiceState};
    let installed = ServiceState::Registration {
        readiness: service.readiness,
        display_name: service.display_name.clone(),
        command: service.command.clone(),
        start: service.start,
    };
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
    ) || owned.is_some() && previous == ServiceState::Absent
    {
        return Ok(Vec::new());
    }
    let owned_configuration = match owned {
        Some(zup_plan::OwnedResource::Service { installed, .. }) if installed == &previous => {
            installed.clone()
        }
        None if previous == ServiceState::Absent => ServiceState::Absent,
        None => return Ok(Vec::new()),
        _ => return Err("service ownership or configuration changed".into()),
    };
    if !payload_changed && service.kind == IntegrationChangeKind::NoOp {
        return Ok(Vec::new());
    }
    let mut operations = Vec::new();
    let mut control = |stage, configuration, before, after, before_execution_generation| {
        operations.push(ServiceControlOperation {
            key: service.key.clone(),
            native_name: native_name.clone(),
            privilege: service.privilege,
            stage,
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
    if running {
        control(
            ServiceControlStage::Quiesce,
            previous.clone(),
            ServiceTerminalState::Running,
            ServiceTerminalState::Stopped,
            execution_generation,
        );
    }
    let restore = if previous == ServiceState::Absent {
        service.lifecycle.install == zup_core::InitialServiceState::Running
    } else {
        running && service.lifecycle.change == zup_core::ChangedServiceState::Preserve
    };
    if restore && service.start != zup_core::ServiceStart::Disabled {
        control(
            ServiceControlStage::Restore,
            installed,
            ServiceTerminalState::Stopped,
            ServiceTerminalState::Running,
            "stopped".into(),
        );
    }
    for operation in &operations {
        operation.validate()?;
    }
    Ok(operations)
}

pub trait NativeServiceManager {
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
    ) -> Result<(), String>;
}

fn check_identity(
    operation: &ServiceControlOperation,
    current: &ServiceObservation,
) -> Result<(), String> {
    operation.validate()?;
    if current.configuration != operation.configuration
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
    check_identity(operation, &current)?;
    if !operation.before.matches(current.runtime)
        || operation.before_execution_generation != current.execution_generation
    {
        return Err(format!(
            "service `{}` runtime changed since authorization",
            operation.native_name
        ));
    }
    manager.transition(operation, operation.after)?;
    let current = manager.observe(operation)?;
    check_identity(operation, &current)?;
    if !operation.after.matches(current.runtime) {
        return Err(format!(
            "service `{}` did not reach {:?}",
            operation.native_name, operation.after
        ));
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
    check_identity(&receipt.operation, &current)?;
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
    check_identity(&receipt.operation, &current)?;
    if receipt.operation.before == ServiceTerminalState::Stopped
        && current.runtime == ServiceRuntimeState::Stopped
    {
        if receipt.completion == ServiceControlCompletion::CompensationOnly
            && !manager.stopped_start_can_be_compensated(&receipt.operation)?
        {
            return Err("incomplete service startup is no longer safe to compensate".into());
        }
        return Ok(());
    }
    verify_service_control(manager, receipt)?;
    manager.transition(&receipt.operation, receipt.operation.before)?;
    let current = manager.observe(&receipt.operation)?;
    check_identity(&receipt.operation, &current)?;
    if !receipt.operation.before.matches(current.runtime) {
        return Err(format!(
            "service `{}` previous runtime could not be restored",
            receipt.operation.native_name
        ));
    }
    Ok(())
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
    let current = manager.observe(operation)?;
    check_identity(operation, &current)?;
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
