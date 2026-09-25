use std::path::PathBuf;

use zup_core::PrerequisiteId;

use crate::model::{
    BootstrapError, BootstrapOperation, BootstrapOperationState, BootstrapOutcome, BootstrapPhase,
    BootstrapPlan, BootstrapState, DetectionResult, PrerequisiteDetector, PrerequisiteProvider,
    ProviderRequest, builtin_for_detector,
};

pub fn recover(
    plan: &BootstrapPlan,
    detector: &impl PrerequisiteDetector,
    state: &mut BootstrapState,
) -> Result<BootstrapOutcome, BootstrapError> {
    state.validate(plan)?;
    for record in &mut state.operations {
        if !matches!(record.state, BootstrapOperationState::Running) {
            continue;
        }
        let operation = plan
            .operation(&record.id)
            .ok_or_else(|| BootstrapError::UnknownOperation(record.id.to_string()))?;
        match detector
            .detect(operation)
            .map_err(|error| BootstrapError::Detector(error.to_string()))?
        {
            DetectionResult::Satisfied { version, evidence } => {
                record.state = BootstrapOperationState::Satisfied { version, evidence };
            }
            DetectionResult::Missing | DetectionResult::Incompatible { .. } => {
                record.state = BootstrapOperationState::Failed {
                    code: "ambiguous_external_process".into(),
                    message: "the prerequisite process ended without a satisfied detector".into(),
                };
            }
        }
    }
    state.recompute();
    if state
        .operations
        .iter()
        .any(|operation| matches!(operation.state, BootstrapOperationState::Failed { .. }))
    {
        state.phase = BootstrapPhase::RecoveryRequired;
        Ok(BootstrapOutcome::RecoveryRequired)
    } else {
        Ok(BootstrapOutcome::Ready)
    }
}

pub fn assess(
    plan: &BootstrapPlan,
    detector: &impl PrerequisiteDetector,
    state: &mut BootstrapState,
) -> Result<Vec<PrerequisiteId>, BootstrapError> {
    state.validate(plan)?;
    if state
        .operations
        .iter()
        .any(|operation| matches!(operation.state, BootstrapOperationState::Failed { .. }))
    {
        return Err(BootstrapError::RecoveryRequired(
            "bootstrap contains a failed or ambiguous operation".into(),
        ));
    }
    for operation in &plan.operations {
        let result = detector
            .detect(operation)
            .map_err(|error| BootstrapError::Detector(error.to_string()))?;
        match result {
            DetectionResult::Satisfied { version, evidence } => {
                state.mark(
                    &operation.id,
                    BootstrapOperationState::Satisfied { version, evidence },
                )?;
            }
            DetectionResult::Missing | DetectionResult::Incompatible { .. } => {
                let current = state
                    .operation_mut(&operation.id)
                    .ok_or_else(|| BootstrapError::UnknownOperation(operation.id.to_string()))?;
                if matches!(current, BootstrapOperationState::RebootRequired { .. }) {
                    continue;
                }
                if matches!(current, BootstrapOperationState::Failed { .. }) {
                    return Err(BootstrapError::RecoveryRequired(
                        "bootstrap contains a failed or ambiguous operation".into(),
                    ));
                }
                *current = BootstrapOperationState::Pending;
            }
        }
    }
    state.recompute();
    Ok(state.remaining.clone())
}

pub fn execute_operation(
    plan: &BootstrapPlan,
    operation: &BootstrapOperation,
    detector: &impl PrerequisiteDetector,
    provider: &impl PrerequisiteProvider,
    executable: PathBuf,
    state: &mut BootstrapState,
) -> Result<BootstrapOutcome, BootstrapError> {
    execute_operation_with_persist(
        plan,
        operation,
        detector,
        provider,
        executable,
        state,
        &mut |_| Ok(()),
    )
}

fn execute_operation_with_persist(
    plan: &BootstrapPlan,
    operation: &BootstrapOperation,
    detector: &impl PrerequisiteDetector,
    provider: &impl PrerequisiteProvider,
    executable: PathBuf,
    state: &mut BootstrapState,
    persist: &mut impl FnMut(&BootstrapState) -> Result<(), BootstrapError>,
) -> Result<BootstrapOutcome, BootstrapError> {
    state.validate(plan)?;
    if let Some(BootstrapOperationState::Failed { code, message }) =
        state.operation_mut(&operation.id)
    {
        return Err(BootstrapError::RecoveryRequired(format!(
            "{code}: {message}"
        )));
    }
    let initial = detector
        .detect(operation)
        .map_err(|error| BootstrapError::Detector(error.to_string()))?;
    if let DetectionResult::Satisfied { version, evidence } = initial {
        state.mark(
            &operation.id,
            BootstrapOperationState::Satisfied { version, evidence },
        )?;
        state.recompute();
        return Ok(BootstrapOutcome::Ready);
    }
    if let Some(BootstrapOperationState::RebootRequired { exit_code }) =
        state.operation_mut(&operation.id)
    {
        return Ok(BootstrapOutcome::RebootRequired {
            exit_code: *exit_code,
            prerequisite_id: operation.id.clone(),
        });
    }
    state.mark(&operation.id, BootstrapOperationState::Running)?;
    state.recompute();
    persist(state)?;
    let outcome = match provider.execute(&ProviderRequest {
        prerequisite_id: operation.id.clone(),
        executable,
        arguments: operation.installer.arguments.clone(),
        expected_digest: operation.package.digest(),
        expected_size: operation.package.size(),
        installer_kind: operation.installer.kind,
        builtin: builtin_for_detector(&operation.detector),
        success_exit_codes: operation.installer.success_exit_codes.clone(),
        reboot_exit_codes: operation.installer.reboot_exit_codes.clone(),
    }) {
        Ok(outcome) => outcome,
        Err(BootstrapError::ProviderPreflight(message)) => {
            state.mark(&operation.id, BootstrapOperationState::Pending)?;
            state.recompute();
            persist(state)?;
            return Err(BootstrapError::ProviderPreflight(message));
        }
        Err(error) => return Err(error),
    };
    if matches!(
        outcome,
        crate::model::ProviderOutcome::RebootRequired { .. }
    ) {
        let exit_code = match outcome {
            crate::model::ProviderOutcome::RebootRequired { exit_code } => exit_code,
            _ => unreachable!(),
        };
        state.mark(
            &operation.id,
            BootstrapOperationState::RebootRequired { exit_code },
        )?;
        state.recompute();
        persist(state)?;
        return Ok(BootstrapOutcome::RebootRequired {
            exit_code,
            prerequisite_id: operation.id.clone(),
        });
    }
    let after = detector
        .detect(operation)
        .map_err(|error| BootstrapError::Detector(error.to_string()))?;
    let DetectionResult::Satisfied { version, evidence } = after else {
        state.mark(
            &operation.id,
            BootstrapOperationState::Failed {
                code: "detector_unsatisfied".into(),
                message: "provider returned success but the requirement is still absent".into(),
            },
        )?;
        state.recompute();
        persist(state)?;
        return Err(BootstrapError::DetectorStillUnsatisfied);
    };
    state.mark(
        &operation.id,
        BootstrapOperationState::Satisfied { version, evidence },
    )?;
    state.recompute();
    persist(state)?;
    Ok(BootstrapOutcome::Ready)
}

pub fn execute_plan(
    plan: &BootstrapPlan,
    detector: &impl PrerequisiteDetector,
    provider: &impl PrerequisiteProvider,
    executable_for: impl FnMut(&BootstrapOperation) -> Result<PathBuf, BootstrapError>,
    state: &mut BootstrapState,
) -> Result<BootstrapOutcome, BootstrapError> {
    execute_plan_with_persist(plan, detector, provider, executable_for, state, &mut |_| {
        Ok(())
    })
}

pub fn execute_plan_with_persist(
    plan: &BootstrapPlan,
    detector: &impl PrerequisiteDetector,
    provider: &impl PrerequisiteProvider,
    mut executable_for: impl FnMut(&BootstrapOperation) -> Result<PathBuf, BootstrapError>,
    state: &mut BootstrapState,
    persist: &mut impl FnMut(&BootstrapState) -> Result<(), BootstrapError>,
) -> Result<BootstrapOutcome, BootstrapError> {
    assess(plan, detector, state)?;
    for operation in &plan.operations {
        if matches!(
            state.operation_mut(&operation.id),
            Some(BootstrapOperationState::Satisfied { .. })
        ) {
            continue;
        }
        let executable = executable_for(operation)?;
        let outcome = execute_operation_with_persist(
            plan, operation, detector, provider, executable, state, persist,
        )?;
        if let BootstrapOutcome::RebootRequired { .. } = outcome {
            return Ok(outcome);
        }
    }
    state.recompute();
    if state.remaining.is_empty() {
        Ok(BootstrapOutcome::Ready)
    } else if let Some((prerequisite_id, exit_code)) =
        state.operations.iter().find_map(|operation| {
            if let BootstrapOperationState::RebootRequired { exit_code } = operation.state {
                Some((operation.id.clone(), exit_code))
            } else {
                None
            }
        })
    {
        Ok(BootstrapOutcome::RebootRequired {
            exit_code,
            prerequisite_id,
        })
    } else {
        Err(BootstrapError::RecoveryRequired(
            "bootstrap did not reach a terminal operation state".into(),
        ))
    }
}
