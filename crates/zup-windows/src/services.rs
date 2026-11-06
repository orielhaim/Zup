use crate::transaction_payload::{BackendReceipt, NativeReconcileResult};
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{Privilege, Target};
use zup_plan::{IntegrationChangeKind, ObservedServiceState, ServiceOperation, ServiceState};

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

pub fn read_service(name: &str, target: &Target) -> Result<ObservedServiceState, ServiceError> {
    crate::scm::query_service(name, target).map_err(ServiceError::Scm)
}

fn state(name: &str, target: &Target) -> Result<ServiceState, ServiceError> {
    crate::scm::query_service(name, target)?
        .configuration()
        .ok_or_else(|| ServiceError::ChangedSincePlanning(name.into()))
}

fn previous(op: &ServiceOperation) -> Result<ServiceState, ServiceError> {
    op.previous
        .configuration()
        .ok_or_else(|| ServiceError::ChangedSincePlanning(op.name.clone()))
}

fn state_target(state: &ServiceState) -> Option<&Target> {
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
        dependencies,
        execution,
        display_name,
        command,
        start,
        readiness,
        failure,
    } = desired
    else {
        return Err(ServiceError::NotExecutable {
            name: name.to_owned(),
            reason: "service registration required".to_owned(),
        });
    };
    failure
        .validate_for(*command.executable.target())
        .map_err(|reason| ServiceError::NotExecutable {
            name: name.into(),
            reason: reason.into(),
        })?;
    if *readiness != zup_core::ServiceReadiness::Manager {
        return Err(ServiceError::NotExecutable {
            name: name.into(),
            reason: "SCM supports manager readiness only".into(),
        });
    }
    let zup_core::ServiceExecution::Windows { account } = execution else {
        return Err(ServiceError::NotExecutable {
            name: name.into(),
            reason: "SCM requires an explicit Windows execution identity".into(),
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
        dependencies: dependencies
            .iter()
            .map(|name| windows_service::service::ServiceDependency::Service(name.into()))
            .collect(),
        account_name: Some(
            match account {
                zup_core::WindowsServiceAccount::LocalService => "NT AUTHORITY\\LocalService",
                zup_core::WindowsServiceAccount::NetworkService => "NT AUTHORITY\\NetworkService",
                zup_core::WindowsServiceAccount::LocalSystem => "LocalSystem",
            }
            .into(),
        ),
        account_password: None,
    })
}

fn write(name: &str, value: &ServiceState, expected: &ServiceState) -> Result<(), ServiceError> {
    let creating = *expected == ServiceState::Absent;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT
            | if creating {
                ServiceManagerAccess::CREATE_SERVICE
            } else {
                ServiceManagerAccess::empty()
            },
    )
    .map_err(|error| backend(name, error))?;
    if creating {
        let info = service_info(name, value, None)?;
        let service = manager
            .create_service(
                &info,
                ServiceAccess::QUERY_CONFIG
                    | ServiceAccess::CHANGE_CONFIG
                    | ServiceAccess::DELETE
                    | ServiceAccess::START,
            )
            .map_err(|error| backend(name, error))?;
        if let Err(error) = service
            .set_config_service_sid_info(windows_service::service::ServiceSidType::Unrestricted)
        {
            return Err(backend(
                name,
                match service.delete() {
                    Ok(()) => format!(
                        "service SID configuration failed: {error}; new registration removed"
                    ),
                    Err(cleanup) => format!(
                        "service SID configuration failed: {error}; registration cleanup failed: {cleanup}; recovery required"
                    ),
                },
            ));
        }
        if let ServiceState::Registration { failure, .. } = value
            && let Err(error) = crate::runtime_failure::apply(&service, *failure)
        {
            let cleanup = service.delete();
            return Err(backend(
                name,
                format!(
                    "failure policy configuration failed: {error}; new registration cleanup: {cleanup:?}"
                ),
            ));
        }
        return Ok(());
    }
    let access = ServiceAccess::QUERY_CONFIG
        | match value {
            ServiceState::Absent => ServiceAccess::DELETE,
            ServiceState::Registration { .. } => {
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::START
            }
        };
    let service = manager
        .open_service(name, access)
        .map_err(|error| backend(name, error))?;
    let target = state_target(expected).ok_or(ServiceError::MissingTarget)?;
    if state(name, target)? != *expected {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    match value {
        ServiceState::Absent => service.delete().map_err(|error| backend(name, error)),
        ServiceState::Registration { .. } => {
            let config = service
                .query_config()
                .map_err(|error| backend(name, error))?;
            let info = service_info(name, value, Some(&config))?;
            service
                .change_config(&info)
                .map_err(|error| backend(name, error))?;
            let ServiceState::Registration { failure, .. } = value else {
                unreachable!()
            };
            crate::runtime_failure::apply(&service, *failure).map_err(|error| backend(name, error))
        }
    }
}

pub fn apply(
    op: &ServiceOperation,
    startup_inhibited: bool,
) -> Result<BackendReceipt, ServiceError> {
    if !matches!(
        op.kind,
        IntegrationChangeKind::Create
            | IntegrationChangeKind::UpdateOwned
            | IntegrationChangeKind::RestoreOwned
            | IntegrationChangeKind::RepairOwned
    ) {
        return Err(ServiceError::NotExecutable {
            name: op.name.clone(),
            reason: "service is not executable".to_owned(),
        });
    }
    let (previous, installed) = transaction_states(op, startup_inhibited)?;
    if state(&op.name, op.command.executable.target())? != previous {
        return Err(ServiceError::ChangedSincePlanning(op.name.clone()));
    }
    if installed != previous {
        write(&op.name, &installed, &previous)?;
    }
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
    write(name, previous, installed)
}

pub fn reconcile(
    op: &ServiceOperation,
    startup_inhibited: bool,
) -> Result<NativeReconcileResult, ServiceError> {
    let current = match state(&op.name, op.command.executable.target()) {
        Ok(state) => state,
        Err(_) => return Ok(NativeReconcileResult::Ambiguous),
    };
    let (previous, installed) = transaction_states(op, startup_inhibited)?;
    Ok(classify_reconcile(
        &op.name,
        op.privilege,
        current,
        previous,
        installed,
    ))
}

fn transaction_states(
    op: &ServiceOperation,
    startup_inhibited: bool,
) -> Result<(ServiceState, ServiceState), ServiceError> {
    let previous = previous(op)?;
    let installed = op.desired_configuration();
    if !startup_inhibited {
        return Ok((previous, installed));
    }
    Ok((
        zup_transaction::disabled_startup(&previous).unwrap_or(previous),
        zup_transaction::disabled_startup(&installed).ok_or(ServiceError::MissingTarget)?,
    ))
}

pub(crate) fn change_startup(
    name: &str,
    expected: &ServiceState,
    desired: &ServiceState,
) -> Result<(), ServiceError> {
    use windows::Win32::System::Services::{
        ChangeServiceConfigW, SC_HANDLE, SERVICE_NO_CHANGE, SERVICE_START_TYPE,
    };
    let target = state_target(expected).ok_or(ServiceError::MissingTarget)?;
    if state(name, target)? != *expected {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    let ServiceState::Registration { start, .. } = desired else {
        return Err(ServiceError::MissingTarget);
    };
    let mut only_start = expected.clone();
    if let ServiceState::Registration { start: value, .. } = &mut only_start {
        *value = *start;
    }
    if &only_start != desired {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| backend(name, e))?;
    let service = manager
        .open_service(name, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| backend(name, e))?;
    // SAFETY: The borrowed SCM handle stays open; null optional arguments leave every
    // property except startup mode unchanged.
    unsafe {
        ChangeServiceConfigW(
            SC_HANDLE(service.raw_handle()),
            windows::Win32::System::Services::ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(start_type(*start) as u32),
            windows::Win32::System::Services::SERVICE_ERROR(SERVICE_NO_CHANGE),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(|e| backend(name, e))?;
    if state(name, target)? != *desired {
        return Err(ServiceError::ChangedAfterInstall(name.into()));
    }
    Ok(())
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
    use zup_core::{Privilege, ResourceKey, ServiceId, ServiceStart, Target};
    use zup_plan::{CommandSpec, TargetPath};

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
            dependencies: Vec::new(),
            failure: Default::default(),
            execution: zup_core::ServiceExecution::Windows {
                account: zup_core::WindowsServiceAccount::LocalService,
            },
            readiness: Default::default(),
            display_name: "Zup Test".into(),
            command: CommandSpec::new(
                TargetPath::new(Target::parse("windows-x64").unwrap(), r"C:\Zup\svc.exe").unwrap(),
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
    #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
    fn uniquely_named_service_create_upgrade_reconcile_and_drift() {
        assert!(crate::transport::is_process_elevated().unwrap());
        let name = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = Cleanup(name.clone());
        let id = ServiceId::new(&name).unwrap();
        let target_triple = Target::parse("windows-x64").unwrap();
        let executable =
            TargetPath::new(target_triple, r"C:\Program Files\Zup Test\service.exe").unwrap();
        let mut op = ServiceOperation {
            failure: Default::default(),
            execution: zup_core::ServiceExecution::Windows {
                account: zup_core::WindowsServiceAccount::LocalService,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service { id },
            kind: IntegrationChangeKind::Create,
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
        assert_eq!(
            reconcile(&op, false).unwrap(),
            NativeReconcileResult::NotApplied
        );
        let first = apply(&op, false).unwrap();
        assert_eq!(
            state(&name, &target_triple).unwrap(),
            op.desired_configuration()
        );
        let mut conflicting = op.desired_configuration();
        if let ServiceState::Registration { display_name, .. } = &mut conflicting {
            *display_name = "Unowned replacement".into();
        }
        assert!(write(&name, &conflicting, &ServiceState::Absent).is_err());
        assert_eq!(
            state(&name, &target_triple).unwrap(),
            op.desired_configuration()
        );
        assert!(matches!(
            reconcile(&op, false).unwrap(),
            NativeReconcileResult::AppliedWithReceipt(_)
        ));
        op.kind = IntegrationChangeKind::UpdateOwned;
        op.previous = crate::scm::query_service(&name, &target_triple).unwrap();
        op.start = ServiceStart::Manual;
        let second = apply(&op, false).unwrap();
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
        write(&name, &foreign, &installed).unwrap();
        assert!(rollback(&name, &previous, &installed).is_err());
        assert_eq!(
            reconcile(&op, false).unwrap(),
            NativeReconcileResult::Ambiguous
        );
    }
}
