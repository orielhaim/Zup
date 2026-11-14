use std::time::Duration;
use windows_service::service::{
    Service, ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod,
};
use zup_core::ServiceFailurePolicy;

pub(crate) fn observe(service: &Service) -> Result<ServiceFailurePolicy, String> {
    let value = service
        .get_failure_actions()
        .map_err(|error| error.to_string())?;
    if value
        .command
        .as_ref()
        .is_some_and(|value| !value.is_empty())
        || value
            .reboot_msg
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || service
            .get_failure_actions_on_non_crash_failures()
            .map_err(|error| error.to_string())?
    {
        return Err("only crash-triggered, bounded SCM restart actions without commands or reboot actions are supported".into());
    }
    let actions = value.actions.unwrap_or_default();
    if actions.is_empty()
        || actions
            .iter()
            .all(|action| action.action_type == ServiceActionType::None)
    {
        return Ok(ServiceFailurePolicy::None);
    }
    let Some((terminal, retries)) = actions.split_last() else {
        unreachable!()
    };
    if terminal.action_type != ServiceActionType::None
        || terminal.delay != Duration::ZERO
        || retries.is_empty()
        || retries.iter().any(|action| {
            action.action_type != ServiceActionType::Restart || action.delay != retries[0].delay
        })
    {
        return Err(
            "SCM recovery requires uniform restart delays and a terminal no-action entry".into(),
        );
    }
    let ServiceFailureResetPeriod::After(reset) = value.reset_period else {
        return Err("SCM retry reset period must be bounded".into());
    };
    let seconds = |duration: Duration| {
        if duration.subsec_nanos() == 0 {
            u32::try_from(duration.as_secs()).ok()
        } else {
            None
        }
    };
    let policy = ServiceFailurePolicy::Windows {
        delay_seconds: seconds(retries[0].delay)
            .ok_or("SCM restart delay must use whole seconds")?,
        restart_attempts: u32::try_from(retries.len())
            .map_err(|_| "too many SCM restart actions")?,
        reset_seconds: seconds(reset).ok_or("SCM reset interval is outside supported bounds")?,
    };
    policy
        .validate_for(zup_core::Target::parse("windows-x64").expect("known target"))
        .map_err(str::to_owned)?;
    Ok(policy)
}

pub(crate) fn apply(service: &Service, policy: ServiceFailurePolicy) -> Result<(), String> {
    let (reset_period, mut actions) = match policy {
        ServiceFailurePolicy::None => {
            (ServiceFailureResetPeriod::After(Duration::ZERO), Vec::new())
        }
        ServiceFailurePolicy::Windows {
            delay_seconds,
            restart_attempts,
            reset_seconds,
        } => {
            policy
                .validate_for(zup_core::Target::parse("windows-x64").expect("known target"))
                .map_err(str::to_owned)?;
            (
                ServiceFailureResetPeriod::After(Duration::from_secs(reset_seconds.into())),
                (0..restart_attempts)
                    .map(|_| ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(delay_seconds.into()),
                    })
                    .collect(),
            )
        }
        _ => return Err("SCM requires a Windows runtime failure policy".into()),
    };
    actions.push(ServiceAction {
        action_type: ServiceActionType::None,
        delay: Duration::ZERO,
    });
    service
        .set_failure_actions_on_non_crash_failures(false)
        .map_err(|error| error.to_string())?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period,
            actions: Some(actions),
            command: Some("".into()),
            reboot_msg: Some("".into()),
        })
        .map_err(|error| error.to_string())?;
    if observe(service)? != policy {
        return Err("SCM failure policy readback differs from authorized configuration".into());
    }
    Ok(())
}
