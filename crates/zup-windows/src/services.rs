use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::Target;
#[cfg(test)]
use zup_plan::{IntegrationChangeKind, ServiceOperation};
use zup_plan::{ObservedServiceState, ServiceState};

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
    #[error("service `{0}` no longer carries this installation's protected ownership marker")]
    OwnershipChanged(String),
    #[error(
        "service `{0}` deletion is pending; wait for its process and outstanding service handles to close, then recover"
    )]
    DeletionPending(String),
    #[error("service `{name}` ownership validation failed: {source}")]
    Ownership {
        name: String,
        source: crate::service_claim::ClaimError,
    },
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

fn state(
    name: &str,
    target: &Target,
    claim: &zup_core::ServiceOwnershipClaim,
) -> Result<ServiceState, ServiceError> {
    let observed = crate::scm::query_service(name, target)?;
    if observed.state != ObservedServiceState::Absent
        && observed.registration
            != (zup_core::ObservedServiceRegistration::Scm {
                claim: claim.clone(),
            })
    {
        return Err(ServiceError::OwnershipChanged(name.into()));
    }
    observed
        .state
        .configuration()
        .ok_or_else(|| ServiceError::ChangedSincePlanning(name.into()))
}

#[cfg(test)]
fn claim_for(op: &ServiceOperation) -> Result<zup_core::ServiceOwnershipClaim, ServiceError> {
    let zup_core::ResourceKey::Service { id } = &op.key else {
        return Err(backend(&op.name, "logical service resource is required"));
    };
    op.registration.claim(id.clone()).ok_or_else(|| {
        backend(
            &op.name,
            "SCM registration requires an installation ownership claim",
        )
    })
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
        state_directories,
        dependencies,
        execution,
        display_name,
        command,
        start,
        readiness,
        failure,
        native_timeouts,
    } = desired
    else {
        return Err(ServiceError::NotExecutable {
            name: name.to_owned(),
            reason: "service registration required".to_owned(),
        });
    };
    if !state_directories.is_empty() {
        return Err(backend(
            name,
            "SCM does not provision systemd state directories",
        ));
    }
    native_timeouts
        .validate_for(execution)
        .map_err(|reason| backend(name, reason))?;
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
    let zup_core::ServiceExecution::WindowsJob { account } = execution else {
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

fn write(
    name: &str,
    value: &ServiceState,
    expected: &ServiceState,
    claim: &zup_core::ServiceOwnershipClaim,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<(), ServiceError> {
    if *expected == ServiceState::Absent {
        let mut info = service_info(name, value, None)?;
        let requested_start = info.start_type;
        info.start_type = ServiceStartType::Disabled;
        let service =
            crate::service_claim::create(&info, claim).map_err(|error| backend(name, error))?;
        if let ServiceState::Registration { failure, .. } = value
            && let Err(error) = crate::runtime_failure::apply(&service, *failure)
        {
            return Err(backend(
                name,
                format!(
                    "failure policy configuration failed: {error}; new disabled registration retained for recovery"
                ),
            ));
        }
        if requested_start != ServiceStartType::Disabled {
            info.start_type = requested_start;
            service
                .change_config(&info)
                .map_err(|error| backend(name, error))?;
        }
        crate::service_claim::confirm(name, &service, claim)
            .map_err(|error| backend(name, error))?;
        lifecycle
            .retain(name, claim)
            .map_err(|error| backend(name, error))?;
        return Ok(());
    }
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| backend(name, error))?;
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
    if state(name, target, claim)? != *expected {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    lifecycle
        .retain(name, claim)
        .map_err(|error| backend(name, error))?;
    match value {
        ServiceState::Absent => {
            crate::service_claim::require_removable(name, &service, claim).map_err(|source| {
                ServiceError::Ownership {
                    name: name.to_owned(),
                    source,
                }
            })?;
            lifecycle
                .release_for_removal(name, &service, claim)
                .map_err(|error| backend(name, error))?;
            service.delete().map_err(|error| backend(name, error))?;
            drop(service);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match state(name, target, claim) {
                    Ok(ServiceState::Absent) => return Ok(()),
                    Ok(_)
                    | Err(ServiceError::Scm(crate::scm::ScmError::DeletionPending { .. })) => {}
                    Err(error) => return Err(error),
                }
                if std::time::Instant::now() >= deadline {
                    return Err(ServiceError::DeletionPending(name.to_owned()));
                }
                std::thread::park_timeout(std::time::Duration::from_millis(20));
            }
        }
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
            crate::runtime_failure::apply(&service, *failure)
                .map_err(|error| backend(name, error))?;
            crate::service_claim::confirm(name, &service, claim)
                .map_err(|error| backend(name, error))
        }
    }
}

fn rollback(
    name: &str,
    previous: &ServiceState,
    installed: &ServiceState,
    claim: &zup_core::ServiceOwnershipClaim,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<(), ServiceError> {
    let target = state_target(installed)
        .or_else(|| state_target(previous))
        .ok_or(ServiceError::MissingTarget)?;
    if state(name, target, claim)? != *installed {
        return Err(ServiceError::ChangedAfterInstall(name.to_owned()));
    }
    if *installed != ServiceState::Absent {
        lifecycle
            .retain(name, claim)
            .map_err(|error| backend(name, error))?;
    }
    if previous.update_from(installed).preserves_runtime() {
        change_live_configuration(name, installed, previous, claim)
    } else {
        write(name, previous, installed, claim, lifecycle)
    }
}

pub(crate) fn change_live_configuration(
    name: &str,
    expected: &ServiceState,
    desired: &ServiceState,
    claim: &zup_core::ServiceOwnershipClaim,
) -> Result<(), ServiceError> {
    use windows::Win32::System::Services::{
        ChangeServiceConfigW, SC_HANDLE, SERVICE_NO_CHANGE, SERVICE_START_TYPE,
    };
    let target = state_target(expected).ok_or(ServiceError::MissingTarget)?;
    if state(name, target, claim)? != *expected {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    let ServiceState::Registration {
        start,
        display_name,
        ..
    } = desired
    else {
        return Err(ServiceError::MissingTarget);
    };
    let mut live = expected.clone();
    let mut startup_type = SERVICE_NO_CHANGE;
    let mut display = None;
    if let ServiceState::Registration {
        start: value,
        display_name: previous_display,
        ..
    } = &mut live
    {
        if value != start {
            startup_type = start_type(*start) as u32;
        }
        *value = *start;
        if previous_display != display_name {
            display = Some(windows::core::HSTRING::from(display_name));
        }
        *previous_display = display_name.clone();
    }
    if &live != desired {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| backend(name, e))?;
    let service = manager
        .open_service(name, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| backend(name, e))?;
    if state(name, target, claim)? != *expected {
        return Err(ServiceError::ChangedSincePlanning(name.into()));
    }
    // SAFETY: The SCM handle and optional display string stay alive through the call.
    unsafe {
        ChangeServiceConfigW(
            SC_HANDLE(service.raw_handle()),
            windows::Win32::System::Services::ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(startup_type),
            windows::Win32::System::Services::SERVICE_ERROR(SERVICE_NO_CHANGE),
            None,
            None,
            None,
            None,
            None,
            None,
            display
                .as_ref()
                .map_or(windows::core::PCWSTR::null(), |value| {
                    windows::core::PCWSTR(value.as_ptr())
                }),
        )
    }
    .map_err(|e| backend(name, e))?;
    if state(name, target, claim)? != *desired {
        return Err(ServiceError::ChangedAfterInstall(name.into()));
    }
    crate::service_claim::confirm(name, &service, claim).map_err(|error| backend(name, error))?;
    Ok(())
}

fn native_context(
    operation: &zup_plan::NativeServiceOperation,
) -> Result<
    (
        String,
        Target,
        zup_core::ServiceOwnershipClaim,
        ServiceState,
        ServiceState,
    ),
    ServiceError,
> {
    let contract = operation.contract();
    let zup_core::ResourceKey::Service { id } = operation.key() else {
        return Err(backend(
            &contract.name,
            "logical service resource is required",
        ));
    };
    let claim = operation
        .registration()
        .claim(id.clone())
        .ok_or_else(|| backend(&contract.name, "SCM ownership claim is required"))?;
    let (before, after) = operation.configurations();
    let before = before.ok_or_else(|| ServiceError::ChangedSincePlanning(contract.name.clone()))?;
    let target = state_target(&after)
        .or_else(|| state_target(&before))
        .copied()
        .ok_or(ServiceError::MissingTarget)?;
    Ok((contract.name, target, claim, before, after))
}

fn native_receipt(
    operation: &zup_plan::NativeServiceOperation,
    configuration: ServiceState,
    claim: zup_core::ServiceOwnershipClaim,
) -> zup_transaction::NativeServiceReceipt {
    zup_transaction::NativeServiceReceipt {
        intent: operation.fingerprint(),
        registration: if configuration == ServiceState::Absent {
            zup_core::ObservedServiceRegistration::Unclaimed
        } else {
            zup_core::ObservedServiceRegistration::Scm { claim }
        },
        configuration,
    }
}

pub(crate) fn apply_native(
    operation: &zup_plan::NativeServiceOperation,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<zup_transaction::OperationReceipt, ServiceError> {
    let (name, target, claim, before, after) = native_context(operation)?;
    if state(&name, &target, &claim)? != before {
        return Err(ServiceError::ChangedSincePlanning(name));
    }
    if before != ServiceState::Absent {
        lifecycle
            .retain(&name, &claim)
            .map_err(|error| backend(&name, error))?;
    }
    if before != after {
        if after.update_from(&before).preserves_runtime() {
            change_live_configuration(&name, &before, &after, &claim)?;
        } else {
            write(&name, &after, &before, &claim, lifecycle)?;
        }
    }
    let receipt = native_receipt(operation, after, claim);
    verify_native(operation, &receipt, lifecycle)?;
    Ok(zup_transaction::OperationReceipt::NativeService(Box::new(
        receipt,
    )))
}

pub(crate) fn verify_native(
    operation: &zup_plan::NativeServiceOperation,
    receipt: &zup_transaction::NativeServiceReceipt,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<(), ServiceError> {
    let (name, target, claim, _, after) = native_context(operation)?;
    if !receipt.matches(operation) {
        return Err(ServiceError::ChangedAfterInstall(name));
    }
    if after != ServiceState::Absent {
        lifecycle
            .retain(&name, &claim)
            .map_err(|error| backend(&name, error))?;
    }
    if state(&name, &target, &claim)? != after {
        return Err(ServiceError::ChangedAfterInstall(name));
    }
    Ok(())
}

pub(crate) fn rollback_native(
    operation: &zup_plan::NativeServiceOperation,
    receipt: &zup_transaction::NativeServiceReceipt,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<(), ServiceError> {
    let (name, _, claim, before, after) = native_context(operation)?;
    if !receipt.matches(operation) {
        return Err(ServiceError::ChangedAfterInstall(name));
    }
    if let zup_plan::NativeServiceOperation::Configure { service, .. } = operation {
        service
            .validate_compensation()
            .map_err(|error| backend(&name, error))?;
    }
    rollback(&name, &before, &after, &claim, lifecycle)
}

pub(crate) fn reconcile_native_rollback(
    operation: &zup_plan::NativeServiceOperation,
    receipt: &zup_transaction::NativeServiceReceipt,
) -> Result<zup_transaction::RollbackReconcileResult, ServiceError> {
    use zup_transaction::RollbackReconcileResult;
    if !receipt.matches(operation) {
        return Ok(RollbackReconcileResult::Ambiguous);
    }
    let (name, target, claim, before, after) = native_context(operation)?;
    Ok(match state(&name, &target, &claim) {
        Ok(current) if current == before => RollbackReconcileResult::Restored,
        Ok(current) if current == after => RollbackReconcileResult::Retry,
        _ => RollbackReconcileResult::Ambiguous,
    })
}

pub(crate) fn reconcile_native(
    operation: &zup_plan::NativeServiceOperation,
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<zup_transaction::ReconcileResult, ServiceError> {
    let (name, target, claim, before, after) = native_context(operation)?;
    let current = match state(&name, &target, &claim) {
        Ok(current) => current,
        Err(_) => return Ok(zup_transaction::ReconcileResult::Ambiguous),
    };
    if current == after {
        if after != ServiceState::Absent {
            lifecycle
                .retain(&name, &claim)
                .map_err(|error| backend(&name, error))?;
            let manager =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                    .map_err(|error| backend(&name, error))?;
            let service = manager
                .open_service(&name, ServiceAccess::QUERY_CONFIG)
                .map_err(|error| backend(&name, error))?;
            crate::service_claim::confirm(&name, &service, &claim)
                .map_err(|error| backend(&name, error))?;
        }
        let receipt = native_receipt(operation, after, claim);
        verify_native(operation, &receipt, lifecycle)?;
        Ok(zup_transaction::ReconcileResult::AppliedWithReceipt(
            zup_transaction::OperationReceipt::NativeService(Box::new(receipt)),
        ))
    } else if current == before {
        Ok(zup_transaction::ReconcileResult::NotApplied)
    } else {
        Ok(zup_transaction::ReconcileResult::Ambiguous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::CommandSpec;
    use zup_core::TargetPath;
    use zup_core::{Privilege, ResourceKey, ServiceId, ServiceStart, Target};

    fn native(op: &ServiceOperation) -> zup_plan::NativeServiceOperation {
        let previous = if op.kind == IntegrationChangeKind::Create {
            None
        } else {
            Some(Box::new(zup_plan::OwnedService {
                registration: op.registration,
                name: op.name.clone(),
                timeouts: op.lifecycle.timeouts_for(&op.execution),
                privilege: op.privilege,
                requires_files: op.requires_files.clone(),
                requires_services: op.requires_services.clone(),
                installed: Box::new(if op.kind == IntegrationChangeKind::RepairOwned {
                    op.desired_configuration()
                } else {
                    op.previous.configuration().unwrap()
                }),
            }))
        };
        zup_plan::NativeServiceOperation::Configure {
            service: Box::new(op.clone()),
            previous,
            activation: zup_plan::ServiceActivation::Declared,
        }
    }

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
            state_directories: Vec::new(),
            registration: zup_core::NativeServiceRegistration::Scm {
                installation: zup_core::InstallationId::new_v7(),
            },
            failure: Default::default(),
            execution: zup_core::ServiceExecution::WindowsJob {
                account: zup_core::WindowsServiceAccount::LocalService,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: zup_core::ServiceLifecycle {
                ..Default::default()
            },
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
        let claim = claim_for(&op).unwrap();
        let mut lifecycle = crate::service_control::ScmLifecycle::default();
        assert_eq!(
            reconcile_native(&native(&op), &mut lifecycle).unwrap(),
            zup_transaction::ReconcileResult::NotApplied
        );
        let first = apply_native(&native(&op), &mut lifecycle).unwrap();
        assert_eq!(
            state(&name, &target_triple, &claim).unwrap(),
            op.desired_configuration()
        );
        let mut conflicting = op.desired_configuration();
        if let ServiceState::Registration { display_name, .. } = &mut conflicting {
            *display_name = "Unowned replacement".into();
        }
        assert!(
            write(
                &name,
                &conflicting,
                &ServiceState::Absent,
                &claim,
                &mut lifecycle
            )
            .is_err()
        );
        assert_eq!(
            state(&name, &target_triple, &claim).unwrap(),
            op.desired_configuration()
        );
        assert!(matches!(
            reconcile_native(&native(&op), &mut lifecycle).unwrap(),
            zup_transaction::ReconcileResult::AppliedWithReceipt(_)
        ));
        op.kind = IntegrationChangeKind::UpdateOwned;
        op.previous = crate::scm::query_service(&name, &target_triple)
            .unwrap()
            .state;
        op.start = ServiceStart::Manual;
        let second = apply_native(&native(&op), &mut lifecycle).unwrap();
        let zup_transaction::OperationReceipt::NativeService(second) = second else {
            panic!("service receipt")
        };
        rollback_native(&native(&op), &second, &mut lifecycle).unwrap();
        let zup_transaction::OperationReceipt::NativeService(first) = first else {
            panic!("service receipt")
        };
        let previous = ServiceState::Absent;
        let installed = first.configuration;
        let service_key = windows_registry::LOCAL_MACHINE
            .create(format!(r"SYSTEM\CurrentControlSet\Services\{name}"))
            .unwrap();
        let parameters = service_key.create("Parameters").unwrap();
        for key in [&service_key, &parameters] {
            key.set_string("ApplicationState", "preserved").unwrap();
            assert!(matches!(
                rollback(&name, &previous, &installed, &claim, &mut lifecycle),
                Err(ServiceError::Ownership {
                    source: crate::service_claim::ClaimError::PersistentData,
                    ..
                })
            ));
            assert_eq!(key.get_string("ApplicationState").unwrap(), "preserved");
            assert_eq!(state(&name, &target_triple, &claim).unwrap(), installed);
            key.remove_value("ApplicationState").unwrap();
        }
        drop(parameters);
        drop(service_key);
        rollback(&name, &previous, &installed, &claim, &mut lifecycle).unwrap();
        assert_eq!(
            state(&name, &target_triple, &claim).unwrap(),
            ServiceState::Absent
        );
        write(&name, &installed, &previous, &claim, &mut lifecycle).unwrap();
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).unwrap();
        let external_handle = manager
            .open_service(&name, ServiceAccess::QUERY_STATUS)
            .unwrap();
        let pending = rollback(&name, &previous, &installed, &claim, &mut lifecycle);
        assert!(
            matches!(pending, Err(ServiceError::DeletionPending(_))),
            "{pending:?}"
        );
        drop(external_handle);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state(&name, &target_triple, &claim).unwrap() != ServiceState::Absent
            && std::time::Instant::now() < deadline
        {
            std::thread::park_timeout(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            state(&name, &target_triple, &claim).unwrap(),
            ServiceState::Absent
        );
        write(&name, &installed, &previous, &claim, &mut lifecycle).unwrap();
        let mut foreign = installed.clone();
        if let ServiceState::Registration { display_name, .. } = &mut foreign {
            *display_name = "Foreign".into();
        }
        write(&name, &foreign, &installed, &claim, &mut lifecycle).unwrap();
        assert!(rollback(&name, &previous, &installed, &claim, &mut lifecycle).is_err());
        assert_eq!(
            reconcile_native(&native(&op), &mut lifecycle).unwrap(),
            zup_transaction::ReconcileResult::Ambiguous
        );
        let mut repair = op.clone();
        repair.kind = IntegrationChangeKind::RepairOwned;
        repair.start = ServiceStart::Disabled;
        repair.previous = crate::scm::query_service(&name, &target_triple)
            .unwrap()
            .state;
        let repaired_operation = native(&repair);
        let repaired = apply_native(&repaired_operation, &mut lifecycle).unwrap();
        let zup_transaction::OperationReceipt::NativeService(repaired) = repaired else {
            panic!("service receipt")
        };
        assert!(rollback_native(&repaired_operation, &repaired, &mut lifecycle).is_err());
        assert_eq!(state(&name, &target_triple, &claim).unwrap(), installed);
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .unwrap();
        manager
            .open_service(&name, ServiceAccess::DELETE)
            .unwrap()
            .delete()
            .unwrap();
        let info = service_info(&name, &installed, None).unwrap();
        assert!(matches!(
            manager.create_service(&info, ServiceAccess::QUERY_CONFIG),
            Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1072)
        ));
        drop(lifecycle);
        let mut lifecycle = crate::service_control::ScmLifecycle::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let recreated = loop {
            match manager.create_service(
                &info,
                ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
            ) {
                Ok(service) => break service,
                Err(windows_service::Error::Winapi(error))
                    if error.raw_os_error() == Some(1072)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(error) => panic!("isolated registration recreation failed: {error}"),
            }
        };
        recreated
            .set_config_service_sid_info(windows_service::service::ServiceSidType::Unrestricted)
            .unwrap();
        let observed = crate::scm::query_service(&name, &target_triple).unwrap();
        assert_eq!(observed.state.configuration().as_ref(), Some(&installed));
        assert_eq!(
            observed.registration,
            zup_core::ObservedServiceRegistration::Unclaimed
        );
        assert_eq!(
            reconcile_native(&native(&op), &mut lifecycle).unwrap(),
            zup_transaction::ReconcileResult::Ambiguous
        );
        assert!(apply_native(&native(&op), &mut lifecycle).is_err());
        assert!(rollback(&name, &previous, &installed, &claim, &mut lifecycle).is_err());
        assert_eq!(
            crate::scm::query_service(&name, &target_triple).unwrap(),
            observed
        );
    }
}
