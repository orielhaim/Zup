use windows_service::service::{ServiceAccess, ServiceStartType, ServiceType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{ServiceStart, Target};
use zup_plan::ObservedServiceState;

use crate::cmdline;

#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    #[error("service manager is unavailable: {0}")]
    Unavailable(String),
    #[error("service `{name}` has an unsupported configuration: {reason}")]
    Unsupported { name: String, reason: String },
    #[error("service `{name}` could not be read: {reason}")]
    Unreadable { name: String, reason: String },
}

pub fn query_service(name: &str, target: &Target) -> Result<ObservedServiceState, ScmError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| ScmError::Unavailable(error.to_string()))?;
    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(ObservedServiceState::Absent);
        }
        Err(error) => {
            return Err(ScmError::Unreadable {
                name: name.to_owned(),
                reason: error.to_string(),
            });
        }
    };
    let config = service
        .query_config()
        .map_err(|error| ScmError::Unreadable {
            name: name.to_owned(),
            reason: error.to_string(),
        })?;
    if config.service_type != ServiceType::OWN_PROCESS {
        return Err(ScmError::Unsupported {
            name: name.to_owned(),
            reason: "service type".to_owned(),
        });
    }
    if config
        .account_name
        .as_ref()
        .and_then(|name| name.to_str())
        .is_none_or(|account| !account.eq_ignore_ascii_case("NT AUTHORITY\\LocalService"))
        || !config.dependencies.is_empty()
    {
        return Err(ScmError::Unsupported { name: name.into(), reason: "only LocalService identity with no SCM dependencies is currently modeled; refusing untracked security or dependency changes".into() });
    }
    let failure = service
        .get_failure_actions()
        .map_err(|error| ScmError::Unreadable {
            name: name.into(),
            reason: error.to_string(),
        })?;
    if failure.actions.as_ref().is_some_and(|actions| {
        actions
            .iter()
            .any(|action| action.action_type != windows_service::service::ServiceActionType::None)
    }) {
        return Err(ScmError::Unsupported {
            name: name.into(),
            reason: "SCM failure actions require an explicit supported runtime-failure policy"
                .into(),
        });
    }
    let command_line = config
        .executable_path
        .into_os_string()
        .into_string()
        .map_err(|_| ScmError::Unsupported {
            name: name.to_owned(),
            reason: "command line contains invalid UTF-16".to_owned(),
        })?;
    let command =
        cmdline::command_spec_from_command_line(&command_line, target).map_err(|error| {
            ScmError::Unsupported {
                name: name.to_owned(),
                reason: error.to_string(),
            }
        })?;
    let display_name = config
        .display_name
        .into_string()
        .map_err(|_| ScmError::Unsupported {
            name: name.to_owned(),
            reason: "display name contains invalid UTF-16".to_owned(),
        })?;
    let start = match config.start_type {
        ServiceStartType::AutoStart => ServiceStart::Automatic,
        ServiceStartType::OnDemand => ServiceStart::Manual,
        ServiceStartType::Disabled => ServiceStart::Disabled,
        _ => {
            return Err(ScmError::Unsupported {
                name: name.to_owned(),
                reason: "start type".to_owned(),
            });
        }
    };
    Ok(ObservedServiceState::Service {
        readiness: Default::default(),
        display_name,
        command,
        start,
        runtime_state: Some(crate::service_control::runtime(
            service
                .query_status()
                .map_err(|error| ScmError::Unreadable {
                    name: name.into(),
                    reason: error.to_string(),
                })?
                .current_state,
        )),
    })
}
