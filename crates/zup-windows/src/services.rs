//! SCM-backed service inspection and managed mutation.

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_exec::{ObservedServiceState, ServiceOperation, ServiceOperationKind, ServiceState};
use zup_transaction::{OperationReceipt, ReconcileResult};

/// Read-only service inspection surface.
pub trait ServiceReader {
    /// Observe service `name`, or `Absent` if it does not exist.
    fn read_service(&self, name: &str) -> Result<ObservedServiceState, String>;
}

/// Production SCM-backed reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsServiceReader;

impl ServiceReader for WindowsServiceReader {
    fn read_service(&self, name: &str) -> Result<ObservedServiceState, String> {
        crate::scm::query_service(name)
    }
}

/// Fake service table for tests.
#[derive(Debug, Default, Clone)]
pub struct FakeServiceReader {
    pub services: std::collections::BTreeMap<String, ObservedServiceState>,
    pub fail: bool,
}

impl ServiceReader for FakeServiceReader {
    fn read_service(&self, name: &str) -> Result<ObservedServiceState, String> {
        if self.fail {
            return Err("access denied".to_owned());
        }
        Ok(self
            .services
            .get(name)
            .cloned()
            .unwrap_or(ObservedServiceState::Absent))
    }
}

fn state(name: &str) -> Result<ServiceState, String> {
    match crate::scm::query_service(name)? {
        ObservedServiceState::Absent => Ok(ServiceState::Absent),
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => Ok(ServiceState::Registration {
            display_name,
            command,
            start,
        }),
    }
}

fn previous(op: &ServiceOperation) -> ServiceState {
    match &op.previous {
        ObservedServiceState::Absent => ServiceState::Absent,
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => ServiceState::Registration {
            display_name: display_name.clone(),
            command: command.clone(),
            start: *start,
        },
    }
}

fn installed(op: &ServiceOperation) -> ServiceState {
    ServiceState::Registration {
        display_name: op.display_name.clone(),
        command: op.command.clone(),
        start: op.start,
    }
}

fn start_type(start: zup_core::ServiceStart) -> ServiceStartType {
    match start {
        zup_core::ServiceStart::Automatic => ServiceStartType::AutoStart,
        zup_core::ServiceStart::Manual => ServiceStartType::OnDemand,
        zup_core::ServiceStart::Disabled => ServiceStartType::Disabled,
    }
}

fn service_info(
    name: &str,
    desired: &ServiceState,
    existing: Option<&windows_service::service::ServiceConfig>,
) -> Result<ServiceInfo, String> {
    let ServiceState::Registration {
        display_name,
        command,
        start,
    } = desired
    else {
        return Err("service registration required".into());
    };
    Ok(ServiceInfo {
        name: name.into(),
        display_name: display_name.into(),
        service_type: existing.map_or(ServiceType::OWN_PROCESS, |config| config.service_type),
        start_type: start_type(*start),
        error_control: existing.map_or(ServiceErrorControl::Normal, |config| config.error_control),
        executable_path: command.executable.as_path().to_path_buf(),
        launch_arguments: command.arguments.iter().map(Into::into).collect(),
        dependencies: existing.map_or_else(Vec::new, |config| config.dependencies.clone()),
        account_name: None,
        account_password: None,
    })
}

fn write(name: &str, value: &ServiceState) -> Result<(), String> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| error.to_string())?;
    match value {
        ServiceState::Absent => {
            let service = manager
                .open_service(name, ServiceAccess::DELETE)
                .map_err(|error| error.to_string())?;
            service.delete().map_err(|error| error.to_string())
        }
        ServiceState::Registration { .. } => {
            let service = manager.open_service(
                name,
                ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
            );
            match service {
                Ok(service) => {
                    let config = service.query_config().map_err(|error| error.to_string())?;
                    let info = service_info(name, value, Some(&config))?;
                    service
                        .change_config(&info)
                        .map_err(|error| error.to_string())
                }
                Err(windows_service::Error::Winapi(error))
                    if error.raw_os_error() == Some(1060) =>
                {
                    let info = service_info(name, value, None)?;
                    manager
                        .create_service(&info, ServiceAccess::QUERY_CONFIG)
                        .map_err(|error| error.to_string())?;
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        }
    }
}

pub fn apply(op: &ServiceOperation) -> Result<OperationReceipt, String> {
    if !matches!(
        op.kind,
        ServiceOperationKind::Create
            | ServiceOperationKind::UpdateOwned
            | ServiceOperationKind::RestoreOwned
    ) {
        return Err("service is not executable".into());
    }
    let previous = previous(op);
    if state(&op.name)? != previous {
        return Err("service changed since planning".into());
    }
    let installed = installed(op);
    write(&op.name, &installed)?;
    Ok(OperationReceipt::Service {
        name: op.name.clone(),
        previous: Box::new(previous),
        installed: Box::new(installed),
    })
}

pub fn rollback(
    name: &str,
    previous: &ServiceState,
    installed: &ServiceState,
) -> Result<(), String> {
    if state(name)? != *installed {
        return Err("service changed after installation".into());
    }
    write(name, previous)
}

pub fn reconcile(op: &ServiceOperation) -> Result<ReconcileResult, String> {
    let current = match state(&op.name) {
        Ok(state) => state,
        Err(_) => return Ok(ReconcileResult::Ambiguous),
    };
    let previous = previous(op);
    let installed = installed(op);
    Ok(classify_reconcile(&op.name, current, previous, installed))
}

fn classify_reconcile(
    name: &str,
    current: ServiceState,
    previous: ServiceState,
    installed: ServiceState,
) -> ReconcileResult {
    if current == installed {
        ReconcileResult::AppliedWithReceipt(OperationReceipt::Service {
            name: name.to_owned(),
            previous: Box::new(previous),
            installed: Box::new(installed),
        })
    } else if current == previous {
        ReconcileResult::NotApplied
    } else {
        ReconcileResult::Ambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use zup_core::{ResourceKey, ServiceId, ServiceStart};
    use zup_platform::{CommandSpec, TargetPath};

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let Ok(manager) =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            else {
                return;
            };
            if let Ok(service) = manager.open_service(&self.0, ServiceAccess::DELETE) {
                let _ = service.delete();
            }
        }
    }

    #[test]
    fn service_reconciliation_is_fail_closed() {
        let previous = ServiceState::Absent;
        let desired = ServiceState::Registration {
            display_name: "Zup Test".into(),
            command: CommandSpec::new(
                TargetPath::new(PathBuf::from(r"C:\Zup\svc.exe")).unwrap(),
                vec![],
            ),
            start: ServiceStart::Disabled,
        };
        assert_eq!(
            classify_reconcile(
                "zup-test",
                previous.clone(),
                previous.clone(),
                desired.clone()
            ),
            ReconcileResult::NotApplied
        );
        assert!(matches!(
            classify_reconcile(
                "zup-test",
                desired.clone(),
                previous.clone(),
                desired.clone()
            ),
            ReconcileResult::AppliedWithReceipt(_)
        ));
        let mut foreign = desired.clone();
        if let ServiceState::Registration { display_name, .. } = &mut foreign {
            *display_name = "Foreign".into();
        }
        assert_eq!(
            classify_reconcile("zup-test", foreign, previous, desired),
            ReconcileResult::Ambiguous
        );
    }

    #[test]
    fn uniquely_named_service_create_upgrade_reconcile_and_drift() {
        if !crate::transport::is_process_elevated().unwrap() {
            eprintln!("service mutation assertions require an elevated test process");
            return;
        }
        let name = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = Cleanup(name.clone());
        let id = ServiceId::new(&name).unwrap();
        let executable =
            TargetPath::new(PathBuf::from(r"C:\Program Files\Zup Test\service.exe")).unwrap();
        let mut op = ServiceOperation {
            key: ResourceKey::Service { id },
            kind: ServiceOperationKind::Create,
            id: name.clone(),
            name: name.clone(),
            display_name: "Zup Test 世界".into(),
            command: CommandSpec::new(
                executable,
                vec!["a b".into(), "say \"hello\"".into(), "世界".into()],
            ),
            start: ServiceStart::Disabled,
            previous: ObservedServiceState::Absent,
            conflict: None,
        };
        assert_eq!(reconcile(&op).unwrap(), ReconcileResult::NotApplied);
        let first = apply(&op).unwrap();
        assert_eq!(state(&name).unwrap(), installed(&op));
        assert!(matches!(
            reconcile(&op).unwrap(),
            ReconcileResult::AppliedWithReceipt(_)
        ));
        op.kind = ServiceOperationKind::UpdateOwned;
        op.previous = crate::scm::query_service(&name).unwrap();
        op.start = ServiceStart::Manual;
        let second = apply(&op).unwrap();
        let OperationReceipt::Service {
            previous,
            installed,
            ..
        } = second
        else {
            panic!("service receipt")
        };
        rollback(&name, &previous, &installed).unwrap();
        let OperationReceipt::Service {
            previous,
            installed,
            ..
        } = first
        else {
            panic!("service receipt")
        };
        let mut foreign = (*installed).clone();
        if let ServiceState::Registration { display_name, .. } = &mut foreign {
            *display_name = "Foreign".into();
        }
        write(&name, &foreign).unwrap();
        assert!(rollback(&name, &previous, &installed).is_err());
        assert_eq!(reconcile(&op).unwrap(), ReconcileResult::Ambiguous);
    }
}
