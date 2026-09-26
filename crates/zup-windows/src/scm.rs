//! Read-only service configuration inspection through SCM.

use windows_service::service::{ServiceAccess, ServiceStartType, ServiceType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{ServiceStart, TargetTriple};
use zup_exec::ObservedServiceState;

use crate::cmdline;

pub fn query_service(name: &str, target: &TargetTriple) -> Result<ObservedServiceState, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| error.to_string())?;
    let service = match manager.open_service(name, ServiceAccess::QUERY_CONFIG) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(ObservedServiceState::Absent);
        }
        Err(error) => return Err(error.to_string()),
    };
    let config = service.query_config().map_err(|error| error.to_string())?;
    if config.service_type != ServiceType::OWN_PROCESS {
        return Err("unsupported service type".to_owned());
    }
    let command_line = config
        .executable_path
        .into_os_string()
        .into_string()
        .map_err(|_| "service command line contains invalid UTF-16".to_owned())?;
    let command = cmdline::command_spec_from_command_line(&command_line, target)?;
    let display_name = config
        .display_name
        .into_string()
        .map_err(|_| "service display name contains invalid UTF-16".to_owned())?;
    let start = match config.start_type {
        ServiceStartType::AutoStart => ServiceStart::Automatic,
        ServiceStartType::OnDemand => ServiceStart::Manual,
        ServiceStartType::Disabled => ServiceStart::Disabled,
        _ => return Err("unsupported service start type".to_owned()),
    };
    Ok(ObservedServiceState::Service {
        display_name,
        command,
        start,
        runtime_state: None,
    })
}
