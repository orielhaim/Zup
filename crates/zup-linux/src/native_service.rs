use std::path::{Path, PathBuf};
use zup_core::{ObservedServiceRegistration, ResourceKey, ServiceExecution, ServiceStart};
use zup_plan::{DeploymentNode, NativeServiceOperation, NodeKind, ServiceState};
use zup_transaction::{NativeServiceReceipt, OperationReceipt, ReconcileResult};

use crate::error::ExecError;
use crate::service_exec::ServiceContext;

fn unit(operation: &NativeServiceOperation) -> Result<String, ExecError> {
    let ResourceKey::Service { .. } = operation.key() else {
        return Err(ExecError::refused(
            "service",
            "service resource identity required",
        ));
    };
    Ok(operation.native_name().to_owned())
}

fn canonical(
    context: &ServiceContext<'_>,
    operation: &NativeServiceOperation,
) -> Result<(String, PathBuf), ExecError> {
    let unit = unit(operation)?;
    let path = crate::machine::authorize_systemd_unit(&unit, context.systemd)
        .map_err(|error| ExecError::refused(&unit, error.to_string()))?;
    crate::service_ops::refuse_source_symlink(&unit, &path)?;
    crate::service_exec::refuse_foreign_integration(&unit, &path)?;
    Ok((unit, path))
}

pub fn prepare(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
) -> Result<(), ExecError> {
    let (unit, _) = canonical(context, operation)?;
    crate::service_ops::require_exec_baseline(context.manager)?;
    for configuration in operation
        .configurations()
        .0
        .iter()
        .chain(std::iter::once(&operation.configurations().1))
    {
        if let ServiceState::Registration { execution, .. } = configuration
            && !matches!(
                execution,
                ServiceExecution::SystemdUser { .. } | ServiceExecution::SystemdDynamicUser
            )
        {
            return Err(ExecError::refused(
                &unit,
                "systemd execution identity required",
            ));
        }
    }
    Ok(())
}

fn stopped(context: &mut ServiceContext<'_>, unit: &str) -> Result<String, ExecError> {
    let generation = context.manager.generation().map_err(ExecError::from)?;
    let runtime = context.manager.runtime(unit).map_err(ExecError::from)?;
    if runtime.state != zup_plan::ServiceRuntimeState::Stopped
        || runtime.pending_job
        || runtime.manager_generation != generation
        || !context
            .manager
            .process_scope_is_empty(unit)
            .map_err(ExecError::from)?
        || context.manager.generation().map_err(ExecError::from)? != generation
    {
        return Err(ExecError::refused(
            unit,
            "registration mutation requires a stable stopped service with no pending job or remaining process",
        ));
    }
    Ok(generation)
}

fn source(unit: &str, path: &Path, state: &ServiceState) -> Result<(), ExecError> {
    let expected = match state {
        ServiceState::Absent => None,
        ServiceState::Registration { .. } => Some(
            crate::services::render_registration(unit, state)
                .map_err(|error| ExecError::refused(unit, error))?,
        ),
    };
    if crate::service_ops::read_canonical_source(path)? != expected {
        return Err(ExecError::refused(
            unit,
            "owned service source differs from its authorized configuration",
        ));
    }
    Ok(())
}

fn observe(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
    desired: &ServiceState,
) -> Result<ServiceState, ExecError> {
    let (unit, path) = canonical(context, operation)?;
    source(&unit, &path, desired)?;
    if *desired == ServiceState::Absent {
        let _ = stopped(context, &unit)?;
        let policy = context
            .manager
            .unit_file_state(&unit)
            .map_err(ExecError::from)?;
        if policy != "not-found" && policy != "disabled" {
            return Err(ExecError::refused(
                &unit,
                "removed source still has native enablement or a mask",
            ));
        }
        return Ok(ServiceState::Absent);
    }
    let ServiceState::Registration { command, .. } = desired else {
        unreachable!()
    };
    let configuration = crate::service_ops::observe_unit(
        &unit,
        &path,
        *command.executable.target(),
        context.manager,
    )?
    .configuration()
    .ok_or_else(|| {
        ExecError::refused(
            &unit,
            "native manager does not recognize the service definition",
        )
    })?;
    let loaded = context.manager.load_unit(&unit).map_err(ExecError::from)?;
    let disabled = matches!(
        &configuration,
        ServiceState::Registration {
            start: ServiceStart::Disabled,
            ..
        }
    );
    if (!disabled && (loaded.load_state != "loaded" || Path::new(&loaded.fragment_path) != path))
        || loaded.need_daemon_reload
        || crate::systemd_policy::validate(&loaded.drop_in_paths).is_err()
    {
        return Err(ExecError::refused(
            &unit,
            "service manager has an unverified fragment or pending reload",
        ));
    }
    Ok(configuration)
}

fn set_configuration(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
    desired: &ServiceState,
) -> Result<(), ExecError> {
    let (unit, path) = canonical(context, operation)?;
    source(&unit, &path, desired)?;
    let policy_only = matches!(desired, ServiceState::Registration { .. })
        && operation.configurations().0.as_ref().is_some_and(|before| {
            matches!(before, ServiceState::Registration { .. })
                && desired.update_from(before).preserves_runtime()
        });
    let mut preserved_execution = None;
    let generation = if policy_only {
        let generation = context.manager.generation().map_err(ExecError::from)?;
        let runtime = context.manager.runtime(&unit).map_err(ExecError::from)?;
        if runtime.pending_job
            || runtime.manager_generation != generation
            || !matches!(
                runtime.state,
                zup_plan::ServiceRuntimeState::Running | zup_plan::ServiceRuntimeState::Stopped
            )
        {
            return Err(ExecError::refused(
                &unit,
                "policy mutation requires a stable native service state",
            ));
        }
        let ServiceState::Registration {
            command,
            execution,
            readiness,
            dependencies,
            ..
        } = desired
        else {
            return Err(ExecError::refused(
                &unit,
                "runtime preservation requires a service registration",
            ));
        };
        let argv = std::iter::once(command.executable.to_string())
            .chain(command.arguments.iter().cloned())
            .collect::<Vec<_>>();
        let loaded = context.manager.load_unit(&unit).map_err(ExecError::from)?;
        if runtime.state == zup_plan::ServiceRuntimeState::Running
            && (runtime.argv.as_ref() != Some(&argv)
                || runtime.execution.as_ref() != Some(execution)
                || runtime.service_type
                    != match readiness {
                        zup_core::ServiceReadiness::Manager => "exec",
                        zup_core::ServiceReadiness::Notify => "notify",
                    }
                || runtime
                    .dependencies
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    != dependencies
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                || loaded.fragment_path != path.to_string_lossy()
                || crate::systemd_policy::validate(&loaded.drop_in_paths).is_err())
        {
            return Err(ExecError::refused(
                &unit,
                "running service differs from the authorized execution",
            ));
        }
        preserved_execution = Some((runtime.state, runtime.execution_generation));
        generation
    } else {
        stopped(context, &unit)?
    };
    context.manager.reload().map_err(ExecError::from)?;
    if let Some((state, execution)) = preserved_execution {
        let runtime = context.manager.runtime(&unit).map_err(ExecError::from)?;
        if runtime.pending_job
            || runtime.state != state
            || runtime.execution_generation != execution
        {
            return Err(ExecError::refused(
                &unit,
                "service execution changed during native configuration reload",
            ));
        }
    }
    let mut changes = Vec::new();
    match desired {
        ServiceState::Registration { command, start, .. } => {
            crate::service_ops::validate_executable_live(
                command,
                context.roots,
                context.expected_uid,
            )?;
            crate::service_exec::apply_policy(&unit, &path, *start, context.manager, &mut changes)?;
        }
        ServiceState::Absent => {
            changes.extend(
                context
                    .manager
                    .remove_owned_enablement(&unit, &path.to_string_lossy())
                    .map_err(ExecError::from)?,
            );
            changes.extend(context.manager.unmask(&unit).map_err(ExecError::from)?);
        }
    }
    crate::service_ops::validate_changes(&unit, &changes)?;
    context.manager.reload().map_err(ExecError::from)?;
    if context.manager.generation().map_err(ExecError::from)? != generation
        || observe(context, operation, desired)? != *desired
    {
        return Err(ExecError::refused(
            &unit,
            "native service configuration could not be verified under the original manager",
        ));
    }
    Ok(())
}

fn receipt(operation: &NativeServiceOperation) -> OperationReceipt {
    let configuration = operation.configurations().1;
    OperationReceipt::NativeService(Box::new(NativeServiceReceipt {
        intent: operation.fingerprint(),
        registration: if configuration == ServiceState::Absent {
            ObservedServiceRegistration::Unclaimed
        } else {
            ObservedServiceRegistration::SourceFile
        },
        configuration,
    }))
}

pub fn apply(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
    source_retained: bool,
) -> Result<OperationReceipt, ExecError> {
    let desired = operation.configurations().1;
    if !source_retained || observe(context, operation, &desired).ok().as_ref() != Some(&desired) {
        set_configuration(context, operation, &desired)?;
    }
    Ok(receipt(operation))
}

pub fn verify(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
    evidence: &OperationReceipt,
) -> Result<(), ExecError> {
    if !matches!(evidence, OperationReceipt::NativeService(receipt) if receipt.matches(operation))
        || observe(context, operation, &operation.configurations().1)?
            != operation.configurations().1
    {
        return Err(ExecError::refused(
            &unit(operation)?,
            "native service receipt differs from its configuration",
        ));
    }
    Ok(())
}

pub fn rollback(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
    evidence: &OperationReceipt,
) -> Result<(), ExecError> {
    if !matches!(evidence, OperationReceipt::NativeService(receipt) if receipt.matches(operation))
        && !matches!(evidence, OperationReceipt::NativeServiceCompensation { intent } if *intent == operation.fingerprint())
    {
        return Err(ExecError::refused(
            &unit(operation)?,
            "service compensation evidence differs",
        ));
    }
    let before = operation.configurations().0.unwrap_or(ServiceState::Absent);
    if observe(context, operation, &before).ok().as_ref() == Some(&before) {
        return Ok(());
    }
    let name = unit(operation)?;
    let observed = context
        .manager
        .unit_file_state(&name)
        .map_err(ExecError::from)?;
    let policy = |state: &ServiceState| match state {
        ServiceState::Absent => "not-found",
        ServiceState::Registration {
            start: ServiceStart::Automatic,
            ..
        } => "enabled",
        ServiceState::Registration {
            start: ServiceStart::Manual,
            ..
        } => "disabled",
        ServiceState::Registration {
            start: ServiceStart::Disabled,
            ..
        } => "masked",
    };
    if observed != policy(&operation.configurations().1) && observed != policy(&before) {
        return Err(ExecError::refused(
            &name,
            "service activation policy changed outside this transaction",
        ));
    }
    set_configuration(context, operation, &before)
}

pub fn reconcile(
    context: &mut ServiceContext<'_>,
    operation: &NativeServiceOperation,
) -> Result<ReconcileResult, ExecError> {
    let desired = operation.configurations().1;
    if observe(context, operation, &desired).ok().as_ref() == Some(&desired) {
        return Ok(ReconcileResult::AppliedWithReceipt(receipt(operation)));
    }
    let (name, path) = canonical(context, operation)?;
    if source(&name, &path, &desired).is_ok() && stopped(context, &name).is_ok() {
        Ok(ReconcileResult::Incomplete)
    } else {
        Ok(ReconcileResult::Ambiguous)
    }
}

pub fn operation(node: &DeploymentNode) -> Result<&NativeServiceOperation, ExecError> {
    match &node.kind {
        NodeKind::NativeService { operation } => Ok(operation),
        _ => Err(ExecError::refused(
            node.id.as_str(),
            "native service effect required",
        )),
    }
}
