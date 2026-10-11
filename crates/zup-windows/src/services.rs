use crate::transaction_payload::{BackendReceipt, NativeReconcileResult};
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{Privilege, TargetTriple};
use zup_exec::{ObservedServiceState, ServiceOperation, ServiceOperationKind, ServiceState};

use crate::lowering::host_path;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Scm(#[from] crate::scm::ScmError),
    #[error("service `{name}` is not executable: {reason}")]
    NotExecutable { name: String, reason: String },
    #[error("service `{0}` changed since planning")]
    ChangedSincePlanning(String),
    #[error("service `{0}` changed after installation")]
    ChangedAfterInstall(String),
    #[error("service target is missing")]
    MissingTarget,
    #[error("service `{name}` could not be written: {reason}")]
    WriteFailed { name: String, reason: String },
}

fn backend(name: &str, reason: impl std::fmt::Display) -> ServiceError {
    ServiceError::WriteFailed {
        name: name.to_owned(),
        reason: reason.to_string(),
    }
}

pub fn read_service(
    name: &str,
    target: &TargetTriple,
) -> Result<ObservedServiceState, ServiceError> {
    crate::scm::query_service(name, target).map_err(ServiceError::Scm)
}

fn state(name: &str, target: &TargetTriple) -> Result<ServiceState, ServiceError> {
    match crate::scm::query_service(name, target)? {
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

fn state_target(state: &ServiceState) -> Option<&TargetTriple> {
    match state {
        ServiceState::Registration { command, .. } => Some(command.executable.target()),
        ServiceState::Absent => None,
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
) -> Result<ServiceInfo, ServiceError> {
    let ServiceState::Registration {
        display_name,
        command,
        start,
    } = desired
    else {
        return Err(ServiceError::NotExecutable {
            name: name.to_owned(),
            reason: "service registration required".to_owned(),
        });
    };
    Ok(ServiceInfo {
        name: name.into(),
        display_name: display_name.into(),
        service_type: existing.map_or(ServiceType::OWN_PROCESS, |config| config.service_type),
        start_type: start_type(*start),
        error_control: existing.map_or(ServiceErrorControl::Normal, |config| config.error_control),
        executable_path: host_path(&command.executable),
        launch_arguments: command.arguments.iter().map(Into::into).collect(),
        dependencies: existing.map_or_else(Vec::new, |config| config.dependencies.clone()),
        account_name: None,
        account_password: None,
    })
}

fn write(name: &str, value: &ServiceState) -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| backend(name, error))?;
    match value {
        ServiceState::Absent => {
            let service = manager
                .open_service(name, ServiceAccess::DELETE)
                .map_err(|error| backend(name, error))?;
            service.delete().map_err(|error| backend(name, error))
        }
        ServiceState::Registration { .. } => {
            let service = manager.open_service(
                name,
                ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
            );
            match service {
                Ok(service) => {
                    let config = service
                        .query_config()
                        .map_err(|error| backend(name, error))?;
                    let info = service_info(name, value, Some(&config))?;
                    service
                        .change_config(&info)
                        .map_err(|error| backend(name, error))
                }
                Err(windows_service::Error::Winapi(error))
                    if error.raw_os_error() == Some(1060) =>
                {
                    let info = service_info(name, value, None)?;
                    manager
                        .create_service(&info, ServiceAccess::QUERY_CONFIG)
                        .map_err(|error| backend(name, error))?;
                    Ok(())
                }
                Err(error) => Err(backend(name, error)),
            }
        }
    }
}

pub fn apply(op: &ServiceOperation) -> Result<BackendReceipt, ServiceError> {
    if !matches!(
        op.kind,
        ServiceOperationKind::Create
            | ServiceOperationKind::UpdateOwned
            | ServiceOperationKind::RestoreOwned
    ) {
        return Err(ServiceError::NotExecutable {
            name: op.name.clone(),
            reason: "service is not executable".to_owned(),
        });
    }
    let previous = previous(op);
    if state(&op.name, op.command.executable.target())? != previous {
        return Err(ServiceError::ChangedSincePlanning(op.name.clone()));
    }
    let installed = installed(op);
    write(&op.name, &installed)?;
    Ok(BackendReceipt::Service {
        name: op.name.clone(),
        privilege: op.privilege,
        previous,
        installed,
    })
}

pub fn rollback(
    name: &str,
    previous: &ServiceState,
    installed: &ServiceState,
) -> Result<(), ServiceError> {
    let target = state_target(installed)
        .or_else(|| state_target(previous))
        .ok_or(ServiceError::MissingTarget)?;
    if state(name, target)? != *installed {
        return Err(ServiceError::ChangedAfterInstall(name.to_owned()));
    }
    write(name, previous)
}

pub fn reconcile(op: &ServiceOperation) -> Result<NativeReconcileResult, ServiceError> {
    let current = match state(&op.name, op.command.executable.target()) {
        Ok(state) => state,
        Err(_) => return Ok(NativeReconcileResult::Ambiguous),
    };
    let previous = previous(op);
    let installed = installed(op);
    Ok(classify_reconcile(
        &op.name,
        op.privilege,
        current,
        previous,
        installed,
    ))
}

fn classify_reconcile(
    name: &str,
    privilege: Privilege,
    current: ServiceState,
    previous: ServiceState,
    installed: ServiceState,
) -> NativeReconcileResult {
    if current == installed {
        NativeReconcileResult::AppliedWithReceipt(Box::new(BackendReceipt::Service {
            name: name.to_owned(),
            privilege,
            previous,
            installed,
        }))
    } else if current == previous {
        NativeReconcileResult::NotApplied
    } else {
        NativeReconcileResult::Ambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction_payload::NativeReconcileResult;
    use zup_core::{Privilege, ResourceKey, ServiceId, ServiceStart, TargetTriple};
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
                TargetPath::new(
                    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
                    r"C:\Zup\svc.exe",
                )
                .unwrap(),
                vec![],
            ),
            start: ServiceStart::Disabled,
        };
        assert_eq!(
            classify_reconcile(
                "zup-test",
                Privilege::System,
                previous.clone(),
                previous.clone(),
                desired.clone()
            ),
            NativeReconcileResult::NotApplied
        );
        assert!(matches!(
            classify_reconcile(
                "zup-test",
                Privilege::System,
                desired.clone(),
                previous.clone(),
                desired.clone()
            ),
            NativeReconcileResult::AppliedWithReceipt(_)
        ));
        let mut foreign = desired.clone();
        if let ServiceState::Registration { display_name, .. } = &mut foreign {
            *display_name = "Foreign".into();
        }
        assert_eq!(
            classify_reconcile("zup-test", Privilege::System, foreign, previous, desired),
            NativeReconcileResult::Ambiguous
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
        let target_triple = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let executable = TargetPath::new(
            target_triple.clone(),
            r"C:\Program Files\Zup Test\service.exe",
        )
        .unwrap();
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
            privilege: Privilege::System,
            previous: ObservedServiceState::Absent,
            conflict: None,
        };
        assert_eq!(reconcile(&op).unwrap(), NativeReconcileResult::NotApplied);
        let first = apply(&op).unwrap();
        assert_eq!(state(&name, &target_triple).unwrap(), installed(&op));
        assert!(matches!(
            reconcile(&op).unwrap(),
            NativeReconcileResult::AppliedWithReceipt(_)
        ));
        op.kind = ServiceOperationKind::UpdateOwned;
        op.previous = crate::scm::query_service(&name, &target_triple).unwrap();
        op.start = ServiceStart::Manual;
        let second = apply(&op).unwrap();
        let BackendReceipt::Service {
            previous,
            installed,
            ..
        } = second
        else {
            panic!("service receipt")
        };
        rollback(&name, &previous, &installed).unwrap();
        let BackendReceipt::Service {
            previous,
            installed,
            ..
        } = first
        else {
            panic!("service receipt")
        };
        let mut foreign = installed.clone();
        if let ServiceState::Registration { display_name, .. } = &mut foreign {
            *display_name = "Foreign".into();
        }
        write(&name, &foreign).unwrap();
        assert!(rollback(&name, &previous, &installed).is_err());
        assert_eq!(reconcile(&op).unwrap(), NativeReconcileResult::Ambiguous);
    }
}
