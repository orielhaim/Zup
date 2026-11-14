use windows_service::service::{ServiceAccess, ServiceStartType, ServiceType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{ServiceStart, Target};
use zup_plan::ObservedServiceState;

use crate::cmdline;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmObservation {
    pub state: ObservedServiceState,
    pub registration: zup_core::ObservedServiceRegistration,
}

#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    #[error("service manager is unavailable: {0}")]
    Unavailable(String),
    #[error("service `{name}` has an unsupported configuration: {reason}")]
    Unsupported { name: String, reason: String },
    #[error("service `{name}` could not be read: {reason}")]
    Unreadable { name: String, reason: String },
    #[error(
        "service `{name}` is marked for deletion; wait for its process and outstanding service handles to close, then recover"
    )]
    DeletionPending { name: String },
}

pub fn query_service(name: &str, target: &Target) -> Result<ScmObservation, ScmError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| ScmError::Unavailable(error.to_string()))?;
    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(ScmObservation {
                state: ObservedServiceState::Absent,
                registration: zup_core::ObservedServiceRegistration::Unclaimed,
            });
        }
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1072) => {
            return Err(ScmError::DeletionPending {
                name: name.to_owned(),
            });
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
    let account = config
        .account_name
        .as_ref()
        .and_then(|value| value.to_str())
        .and_then(|value| match value.to_ascii_lowercase().as_str() {
            "nt authority\\localservice" => Some(zup_core::WindowsServiceAccount::LocalService),
            "nt authority\\networkservice" => Some(zup_core::WindowsServiceAccount::NetworkService),
            "localsystem" => Some(zup_core::WindowsServiceAccount::LocalSystem),
            _ => None,
        })
        .ok_or_else(|| ScmError::Unsupported {
            name: name.into(),
            reason: "only explicitly modeled credential-free SCM accounts are supported".into(),
        })?;
    let dependencies = config
        .dependencies
        .iter()
        .map(|dependency| match dependency {
            windows_service::service::ServiceDependency::Service(name) => name
                .to_str()
                .map(|name| name.to_lowercase())
                .ok_or_else(|| ScmError::Unsupported {
                    name: name.to_string_lossy().into_owned(),
                    reason: "invalid service dependency name".into(),
                }),
            _ => Err(ScmError::Unsupported {
                name: name.into(),
                reason: "SCM load-order group dependencies are not managed services".into(),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if service
        .get_config_service_sid_info()
        .map_err(|error| ScmError::Unreadable {
            name: name.into(),
            reason: error.to_string(),
        })?
        != windows_service::service::ServiceSidType::Unrestricted
    {
        return Err(ScmError::Unsupported {
            name: name.into(),
            reason: "managed services require an unrestricted service SID".into(),
        });
    }
    let failure =
        crate::runtime_failure::observe(&service).map_err(|reason| ScmError::Unsupported {
            name: name.into(),
            reason,
        })?;
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
    let registration =
        crate::service_claim::observe(name, &service).map_err(|error| ScmError::Unreadable {
            name: name.into(),
            reason: error.to_string(),
        })?;
    Ok(ScmObservation {
        registration,
        state: ObservedServiceState::Service {
            state_directories: Vec::new(),
            native_timeouts: zup_core::NativeServiceTimeouts::Scm,
            dependencies,
            failure,
            execution: zup_core::ServiceExecution::WindowsJob { account },
            readiness: Default::default(),
            display_name,
            command: Box::new(command),
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
        },
    })
}
