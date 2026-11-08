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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceControlIntent {
    Transition,
    Preserve,
    Reobserve,
}

/// Authorized runtime intent. Native adapters must verify the complete registration
/// and manager generation, not identify a service by its process id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlOperation {
    pub intent: ServiceControlIntent,
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
    pub timeouts: zup_core::ServiceTimeouts,
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
            || self.dependencies.len() > crate::MAX_BACKEND_DEPENDENCIES
        {
            return Err(
                "invalid service control identity, registration, generation or deadline".into(),
            );
        }
        let ServiceState::Registration {
            execution,
            native_timeouts,
            ..
        } = &self.configuration
        else {
            return Err("service control requires a registration".into());
        };
        self.timeouts
            .validate_for(execution)
            .map_err(str::to_owned)?;
        if self.intent == ServiceControlIntent::Transition
            && self.timeouts.native() != *native_timeouts
        {
            return Err("service deadline policy differs from its native registration".into());
        }
        if self.owned_configuration == ServiceState::Absent
            && self.stage != ServiceControlStage::Restore
        {
            return Err("an unregistered service cannot be quiesced".into());
        }
        let configuration_transition = self.before_configuration != self.configuration;
        let mut dependencies = std::collections::BTreeSet::new();
        if self.dependencies.iter().any(|key| {
            !matches!(key, ResourceKey::Service { .. })
                || key == &self.key
                || !dependencies.insert(key)
        }) {
            return Err("invalid service dependency".into());
        }
        if self.intent == ServiceControlIntent::Preserve {
            if self.before != self.after
                || configuration_transition
                || self.owned_configuration == ServiceState::Absent
                || self.configuration != self.owned_configuration
                    && !self
                        .configuration
                        .update_from(&self.owned_configuration)
                        .preserves_runtime()
            {
                return Err(
                    "runtime preservation cannot change registration or runtime state".into(),
                );
            }
            return Ok(());
        }
        if self.intent == ServiceControlIntent::Reobserve {
            if self.stage != ServiceControlStage::Restore
                || self.before != ServiceTerminalState::Stopped
                || self.after != ServiceTerminalState::Stopped
                || self.before_execution_generation != "stopped"
                || configuration_transition
                || self.configuration != self.owned_configuration
            {
                return Err("registration repair requires a stopped runtime reobservation".into());
            }
            return Ok(());
        }
        if self.stage == ServiceControlStage::Quiesce
            && matches!(self.configuration, ServiceState::Registration {
                failure: zup_core::ServiceFailurePolicy::Windows { .. },
                start,
                ..
            } if start != zup_core::ServiceStart::Disabled)
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
                || self.privilege != Privilege::System
            {
                return Err("service control can only inhibit or restore native activation".into());
            }
        }
        let already_inhibited = self.stage == ServiceControlStage::Quiesce
            && self.before == ServiceTerminalState::Stopped
            && self.after == ServiceTerminalState::Stopped
            && disabled_startup(&self.configuration).as_ref() == Some(&self.configuration);
        if (self.before == self.after && !configuration_transition && !already_inhibited)
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
        Ok(())
    }

    pub fn timeout_for(&self, state: ServiceTerminalState) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(match state {
            ServiceTerminalState::Running => self.timeouts.start_observation_seconds(),
            ServiceTerminalState::Stopped => self.timeouts.stop_observation_seconds(),
        }))
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
    matches!(state, ServiceState::Registration { start, .. } if *start != zup_core::ServiceStart::Disabled)
}

pub fn validate_owned_service_inputs(
    plan: &crate::TransactionPlan,
    ledger: Option<&zup_plan::InstallLedger>,
) -> Result<(), String> {
    let Some(ledger) = ledger else {
        return Ok(());
    };
    for (key, owned) in &ledger.resources {
        let zup_plan::OwnedResource::Service {
            installed,
            requires_files,
            ..
        } = owned
        else {
            continue;
        };
        let ServiceState::Registration {
            command,
            dependencies,
            ..
        } = installed.as_ref()
        else {
            return Err("owned service lacks its installed registration".into());
        };
        let input_changed = std::iter::once(&command.executable)
            .chain(requires_files)
            .any(|path| {
                let protected = ResourceKey::File {
                    destination: path.to_string(),
                }
                .canonical_for(&plan.target);
                plan.nodes.iter().any(|node| {
                    matches!(&node.kind,
                crate::NodeKind::FileMutation { key, .. } | crate::NodeKind::FileRemoval { key }
                if key.canonical_for(&plan.target) == protected)
                })
            });
        let dependency_changed = plan.nodes.iter().any(|node| {
            matches!(&node.kind,
            crate::NodeKind::ServiceControl { operation }
            if operation.stage == ServiceControlStage::Quiesce
                && operation.intent == ServiceControlIntent::Transition
                && dependencies.contains(&operation.native_name))
        });
        if !input_changed && !dependency_changed {
            continue;
        }
        let inhibited = disabled_startup(installed).ok_or("owned service registration required")?;
        if !plan.nodes.iter().any(|node| {
            matches!(&node.kind,
            crate::NodeKind::ServiceControl { operation }
            if &operation.key == key
                && operation.intent == ServiceControlIntent::Transition
                && operation.stage == ServiceControlStage::Quiesce
                && operation.owned_configuration == **installed
                && operation.before_configuration == **installed
                && operation.configuration == inhibited
                && operation.after == ServiceTerminalState::Stopped)
        }) {
            return Err(format!(
                "service {key:?} requires verified native inhibition before its inputs change"
            ));
        }
    }
    Ok(())
}

pub fn validate_startup_inhibition(
    plan: &crate::TransactionPlan,
    key: &ResourceKey,
    previous: &ServiceState,
    desired: &ServiceState,
    protected_files: &[zup_plan::TargetPath],
    guarded: bool,
) -> Result<(), String> {
    let input_changed = protected_files.iter().any(|path| {
        let file = ResourceKey::File {
            destination: path.to_string(),
        }
        .canonical_for(&plan.target);
        plan.nodes.iter().any(|node| {
            matches!(&node.kind,
            crate::NodeKind::FileMutation { key, .. } | crate::NodeKind::FileRemoval { key }
            if key.canonical_for(&plan.target) == file)
        })
    });
    let dependency_changed = [previous, desired].into_iter().any(|state| {
        let ServiceState::Registration { dependencies, .. } = state else { return false; };
        plan.nodes.iter().any(|node| matches!(&node.kind,
            crate::NodeKind::ServiceControl { operation }
            if operation.intent == ServiceControlIntent::Transition && dependencies.contains(&operation.native_name)))
    });
    let replacement = !desired.update_from(previous).preserves_runtime();
    let activation_changes =
        requires_startup_inhibition(previous) || requires_startup_inhibition(desired);
    if (input_changed || dependency_changed || replacement) && activation_changes && !guarded {
        return Err("service replacement requires native activation inhibition".into());
    }
    if !guarded {
        return Ok(());
    }
    let control_exists = |stage, before: &ServiceState, after: &ServiceState| {
        plan.nodes.iter().any(|node| {
            matches!(&node.kind,
            crate::NodeKind::ServiceControl { operation }
            if &operation.key == key && operation.stage == stage
                && operation.intent == ServiceControlIntent::Transition
                && &operation.before_configuration == before && &operation.configuration == after)
        })
    };
    if requires_startup_inhibition(previous)
        && !control_exists(
            ServiceControlStage::Quiesce,
            previous,
            &disabled_startup(previous).expect("registration"),
        )
    {
        return Err("activation inhibition is missing its authorized quiesce node".into());
    }
    if requires_startup_inhibition(desired)
        && !control_exists(
            ServiceControlStage::Restore,
            &disabled_startup(desired).expect("registration"),
            desired,
        )
    {
        return Err("activation inhibition is missing its authorized restore node".into());
    }
    Ok(())
}

pub fn validate_runtime_preservation(
    plan: &crate::TransactionPlan,
    operation: &ServiceControlOperation,
    protected_files: &[zup_plan::TargetPath],
) -> Result<(), String> {
    if operation.intent == ServiceControlIntent::Transition {
        return Ok(());
    }
    if operation.intent == ServiceControlIntent::Preserve {
        let other = plan
            .nodes
            .iter()
            .find_map(|node| match &node.kind {
                crate::NodeKind::ServiceControl { operation: other }
                    if other.key == operation.key && other.stage != operation.stage =>
                {
                    Some(other)
                }
                _ => None,
            })
            .ok_or("runtime preservation requires observations before and after mutation")?;
        let (before, after) = if operation.stage == ServiceControlStage::Quiesce {
            (operation, other.as_ref())
        } else {
            (other.as_ref(), operation)
        };
        if other.intent != ServiceControlIntent::Preserve
            || before.configuration != before.owned_configuration
                && !(after.configuration == before.owned_configuration
                    && before
                        .configuration
                        .update_from(&before.owned_configuration)
                        .preserves_runtime())
            || !after
                .configuration
                .update_from(&before.configuration)
                .preserves_runtime()
            || before.before != after.before
            || before.before_execution_generation != after.before_execution_generation
            || before.manager_generation != after.manager_generation
            || before.owned_configuration != after.owned_configuration
        {
            return Err(
                "runtime preservation requires changes compatible with the same execution".into(),
            );
        }
    }
    let ServiceState::Registration {
        command,
        dependencies,
        ..
    } = &operation.configuration
    else {
        return Err("runtime preservation requires a registration".into());
    };
    if plan.nodes.iter().any(|node| {
        matches!(&node.kind,
        crate::NodeKind::ServiceControl { operation: dependency }
            if dependency.intent == ServiceControlIntent::Transition
                && dependencies.contains(&dependency.native_name))
    }) {
        return Err("runtime preservation cannot cross a dependency runtime replacement".into());
    }
    for path in std::iter::once(&command.executable).chain(protected_files) {
        let protected = ResourceKey::File {
            destination: path.to_string(),
        }
        .canonical_for(&plan.target);
        if plan.nodes.iter().any(|node| {
            matches!(&node.kind,
            crate::NodeKind::FileMutation { key, .. } | crate::NodeKind::FileRemoval { key }
                if key.canonical_for(&plan.target) == protected)
        }) {
            return Err("runtime preservation cannot replace or remove a service input".into());
        }
    }
    Ok(())
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
    service
        .lifecycle
        .validate(service.start, &service.execution)?;
    use zup_plan::ObservedServiceState;
    let installed = service.desired_configuration();
    let previous = service
        .previous
        .configuration()
        .ok_or("unrecognized service registration")?;
    let desired_timeouts = service.lifecycle.timeouts_for(&service.execution);
    let previous_timeouts = match owned {
        Some(zup_plan::OwnedResource::Service { timeouts, .. }) => *timeouts,
        _ => desired_timeouts,
    };
    let owned_configuration = match owned {
        Some(zup_plan::OwnedResource::Service { installed, .. })
            if installed.as_ref() == &previous
                || service.kind == zup_plan::IntegrationChangeKind::RestoreOwned
                    && previous == ServiceState::Absent
                    && installed.as_ref() == &service.desired_configuration()
                || service.kind == zup_plan::IntegrationChangeKind::RepairOwned
                    && installed.as_ref() == &service.desired_configuration()
                    && service.preserves_runtime() =>
        {
            installed.as_ref().clone()
        }
        None if previous == ServiceState::Absent => ServiceState::Absent,
        None => return Ok(Vec::new()),
        _ => return Err("service ownership or configuration changed".into()),
    };
    if !payload_changed
        && previous == installed
        && service.kind == zup_plan::IntegrationChangeKind::NoOp
    {
        return Ok(Vec::new());
    }
    if !payload_changed
        && service.preserves_runtime()
        && service.lifecycle.change == zup_core::ChangedServiceState::Preserve
    {
        let state = match service.previous {
            ObservedServiceState::Service {
                runtime_state: Some(ServiceRuntimeState::Running),
                ..
            } => ServiceTerminalState::Running,
            ObservedServiceState::Service {
                runtime_state: Some(ServiceRuntimeState::Stopped),
                ..
            } => ServiceTerminalState::Stopped,
            _ => {
                return Err(
                    "runtime preservation requires an authoritative terminal observation".into(),
                );
            }
        };
        return [
            (ServiceControlStage::Quiesce, previous),
            (ServiceControlStage::Restore, installed),
        ]
        .into_iter()
        .map(|(stage, configuration)| {
            let operation = ServiceControlOperation {
                intent: ServiceControlIntent::Preserve,
                key: service.key.clone(),
                native_name: native_name.clone(),
                privilege: service.privilege,
                stage,
                before_configuration: configuration.clone(),
                configuration,
                owned_configuration: owned_configuration.clone(),
                before: state,
                after: state,
                manager_generation: manager_generation.clone(),
                before_execution_generation: execution_generation.clone(),
                timeouts: match stage {
                    ServiceControlStage::Quiesce => previous_timeouts,
                    ServiceControlStage::Restore => desired_timeouts,
                },
                dependencies: Vec::new(),
            };
            operation.validate()?;
            Ok(operation)
        })
        .collect();
    }
    let mut operations = Vec::new();
    let mut control =
        |stage, before_configuration, configuration, before, after, before_execution_generation| {
            operations.push(ServiceControlOperation {
                intent: ServiceControlIntent::Transition,
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
                timeouts: match stage {
                    ServiceControlStage::Quiesce => previous_timeouts,
                    ServiceControlStage::Restore => desired_timeouts,
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
    if previous != ServiceState::Absent {
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
        owned.is_none() && service.lifecycle.install == zup_core::InitialServiceState::Running
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
    let receipt = crate::OperationReceipt::ServiceControl(Box::new(ServiceControlReceipt {
        operation: operation.clone(),
        completion: ServiceControlCompletion::Reached,
        execution_generation: current.execution_generation,
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
    check_deadline(deadline)
}

pub fn rollback_service_control(
    manager: &mut dyn NativeServiceManager,
    receipt: &ServiceControlReceipt,
) -> Result<(), String> {
    receipt.operation.validate()?;
    let deadline =
        std::time::Instant::now() + receipt.operation.timeout_for(receipt.operation.before);
    if receipt.operation.intent != ServiceControlIntent::Transition {
        return verify_service_control_before(manager, receipt, deadline);
    }
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
    if inverse.owned_configuration == ServiceState::Absent {
        inverse.owned_configuration = receipt.operation.configuration.clone();
    }
    check_deadline(deadline)?;
    manager.transition(&inverse, inverse.after, &current, deadline)?;
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
                    execution_generation: current.execution_generation,
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
                execution_generation: current.execution_generation,
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
        check_deadline(deadline)?;
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
