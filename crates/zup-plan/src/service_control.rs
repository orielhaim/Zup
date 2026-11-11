use crate::{ServiceRuntimeState, ServiceState};
use serde::{Deserialize, Serialize};
use zup_core::{Privilege, ResourceKey};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceScopeObservation {
    NativeManager,
    WindowsJob {
        boot_sequence: u32,
        state: WindowsJobState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowsJobState {
    Absent,
    Empty,
    Member { execution_generation: String },
    Occupied { processes: std::num::NonZeroU32 },
}

/// Authorized runtime intent. Native adapters must verify the complete registration
/// and manager generation, not identify a service by its process id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlOperation {
    pub registration: zup_core::NativeServiceRegistration,
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
    pub process_scope: ServiceScopeObservation,
    pub timeouts: zup_core::ServiceTimeouts,
    pub dependencies: Vec<ResourceKey>,
    pub requires_files: Vec<zup_core::TargetPath>,
}

impl ServiceScopeObservation {
    pub fn validate(
        &self,
        registration: zup_core::NativeServiceRegistration,
    ) -> Result<(), String> {
        match (self, registration) {
            (Self::NativeManager, zup_core::NativeServiceRegistration::SourceFile)
            | (
                Self::WindowsJob {
                    state:
                        WindowsJobState::Absent
                        | WindowsJobState::Empty
                        | WindowsJobState::Occupied { .. },
                    ..
                },
                zup_core::NativeServiceRegistration::Scm { .. },
            ) => Ok(()),
            (
                Self::WindowsJob {
                    state:
                        WindowsJobState::Member {
                            execution_generation,
                        },
                    ..
                },
                zup_core::NativeServiceRegistration::Scm { .. },
            ) if !execution_generation.is_empty()
                && execution_generation.len() <= 256
                && !execution_generation.chars().any(char::is_control) =>
            {
                Ok(())
            }
            _ => Err("service process scope does not match its native manager".into()),
        }
    }
}

impl ServiceControlOperation {
    pub fn guards_node(&self, node: &crate::DeploymentNode) -> bool {
        match &node.kind {
            crate::NodeKind::FileMutation { key, .. } | crate::NodeKind::FileRemoval { key } => {
                node.file_destination()
                    .is_some_and(|path| self.protects(path))
                    || matches!((key, &self.key),
                        (ResourceKey::File { id: zup_core::FileResourceId::Native { id: zup_core::NativeFileId::ServiceSource { id } } }, ResourceKey::Service { id: service }) if id == service)
            }
            crate::NodeKind::NativeService { operation } => operation.key() == &self.key,
            _ => false,
        }
    }

    pub fn protects(&self, path: &zup_core::TargetPath) -> bool {
        self.requires_files.iter().any(|required| required.equivalent(path))
            || [&self.before_configuration, &self.configuration, &self.owned_configuration]
                .iter().any(|configuration| matches!(configuration, ServiceState::Registration { command, .. } if command.executable.equivalent(path)))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.process_scope.validate(self.registration)?;
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
                if matches!(
                    self.registration,
                    zup_core::NativeServiceRegistration::Scm { .. }
                ) != matches!(execution, zup_core::ServiceExecution::WindowsJob { .. })
                {
                    return Err("service ownership proof does not match its native manager".into());
                }
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
    plan: &crate::DeploymentPlan,
    ledger: Option<&crate::InstallationRecord>,
) -> Result<(), String> {
    let Some(ledger) = ledger else {
        return Ok(());
    };
    for (key, owned) in &ledger.current.resources {
        let crate::OwnedResource::Service(crate::OwnedService {
            installed,
            requires_files,
            ..
        }) = owned
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
            .any(|path| plan.nodes.iter().any(|node| node.mutates_file(path)));
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
                && requires_files.iter().all(|path| operation.protects(path))
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
    plan: &crate::DeploymentPlan,
    key: &ResourceKey,
    previous: &ServiceState,
    desired: &ServiceState,
    protected_files: &[zup_core::TargetPath],
    guarded: bool,
) -> Result<(), String> {
    let input_changed = protected_files
        .iter()
        .any(|path| plan.nodes.iter().any(|node| node.mutates_file(path)));
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
    plan: &crate::DeploymentPlan,
    operation: &ServiceControlOperation,
    protected_files: &[zup_core::TargetPath],
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
        if plan.nodes.iter().any(|node| node.mutates_file(path)) {
            return Err("runtime preservation cannot replace or remove a service input".into());
        }
    }
    Ok(())
}

pub fn plan_service_controls(
    service: &crate::ServiceOperation,
    owned: Option<&crate::OwnedResource>,
    payload_changed: bool,
    native_name: String,
    manager_generation: String,
    execution_generation: String,
    process_scope: ServiceScopeObservation,
) -> Result<Vec<ServiceControlOperation>, String> {
    service
        .lifecycle
        .validate(service.start, &service.execution)?;
    use crate::ObservedServiceState;
    let installed = service.desired_configuration();
    let mut requires_files = service.requires_files.clone();
    if let Some(crate::OwnedResource::Service(owned)) = owned {
        for path in &owned.requires_files {
            if !requires_files
                .iter()
                .any(|required| required.equivalent(path))
            {
                requires_files.push(path.clone());
            }
        }
    }
    let previous = service
        .previous
        .configuration()
        .ok_or("unrecognized service registration")?;
    let desired_timeouts = service.lifecycle.timeouts_for(&service.execution);
    let previous_timeouts = match owned {
        Some(crate::OwnedResource::Service(crate::OwnedService { timeouts, .. })) => *timeouts,
        _ => desired_timeouts,
    };
    let owned_configuration = match owned {
        Some(crate::OwnedResource::Service(crate::OwnedService { installed, .. }))
            if installed.as_ref() == &previous
                || service.kind == crate::IntegrationChangeKind::RestoreOwned
                    && previous == ServiceState::Absent
                    && installed.as_ref() == &service.desired_configuration()
                || service.kind == crate::IntegrationChangeKind::RepairOwned
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
        && service.kind == crate::IntegrationChangeKind::NoOp
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
                requires_files: requires_files.clone(),
                registration: service.registration,
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
                process_scope: process_scope.clone(),
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
                requires_files: requires_files.clone(),
                registration: service.registration,
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
                process_scope: process_scope.clone(),
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

pub fn bind_service_dependencies(
    controls: &mut [ServiceControlOperation],
    execution: &crate::ExecutionPlan,
    ledger: Option<&crate::InstallationRecord>,
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
                                crate::ObservedServiceState::Service {
                                    runtime_state: Some(crate::ServiceRuntimeState::Running),
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
