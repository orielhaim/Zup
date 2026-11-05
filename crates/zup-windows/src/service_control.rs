use std::time::{Duration, Instant};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_transaction::{
    NativeServiceManager, ServiceControlOperation, ServiceObservation, ServiceTerminalState,
};

pub struct ScmLifecycle;

pub fn manager_generation() -> Result<String, String> {
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS,
    };
    #[repr(C)]
    #[derive(Default)]
    struct BootEnvironment {
        identifier: windows::core::GUID,
        _firmware_type: u32,
        _padding: u32,
        _boot_flags: u64,
    }
    const {
        assert!(std::mem::size_of::<BootEnvironment>() == 32);
        assert!(std::mem::offset_of!(BootEnvironment, _firmware_type) == 16);
        assert!(std::mem::offset_of!(BootEnvironment, _boot_flags) == 24);
    }
    // Native layout: https://github.com/winsiderss/phnt/blob/master/ntexapi.h
    const BOOT_ENVIRONMENT: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(90);
    let mut information = BootEnvironment::default();
    let mut length = 0;
    // SAFETY: The kernel receives an aligned, initialized 32-byte output structure.
    let status = unsafe {
        NtQuerySystemInformation(
            BOOT_ENVIRONMENT,
            (&mut information as *mut BootEnvironment).cast(),
            std::mem::size_of::<BootEnvironment>() as u32,
            &mut length,
        )
    };
    if status.0 < 0
        || length as usize != std::mem::size_of::<BootEnvironment>()
        || information.identifier == windows::core::GUID::default()
    {
        return Err(format!(
            "native Windows boot identity is unavailable: {status:?}"
        ));
    }
    Ok(format!("{:?}", information.identifier))
}

fn open(name: &str, access: ServiceAccess) -> Result<windows_service::service::Service, String> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|manager| manager.open_service(name, access))
        .map_err(|error| format!("SCM service `{name}`: {error}"))
}

pub fn runtime(state: ServiceState) -> zup_plan::ServiceRuntimeState {
    use zup_plan::ServiceRuntimeState as Runtime;
    match state {
        ServiceState::Stopped => Runtime::Stopped,
        ServiceState::StartPending => Runtime::StartPending,
        ServiceState::StopPending => Runtime::StopPending,
        ServiceState::Running => Runtime::Running,
        ServiceState::ContinuePending => Runtime::ContinuePending,
        ServiceState::PausePending => Runtime::PausePending,
        ServiceState::Paused => Runtime::Paused,
    }
}

fn execution_generation(pid: Option<u32>) -> Result<String, String> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let Some(pid) = pid else {
        return Ok("stopped".into());
    };
    // SAFETY: Query-only access; the returned handle is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| e.to_string())?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        result.map_err(|e| e.to_string())?;
        Ok(format!(
            "{pid}:{}:{}",
            creation.dwHighDateTime, creation.dwLowDateTime
        ))
    }
}

impl NativeServiceManager for ScmLifecycle {
    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String> {
        let target = match &operation.configuration {
            zup_plan::ServiceState::Registration { command, .. } => command.executable.target(),
            _ => return Err("service control requires registration".into()),
        };
        let registration =
            crate::scm::query_service(&operation.native_name, target).map_err(|e| e.to_string())?;
        let configuration = registration
            .configuration()
            .ok_or("unrecognized SCM registration")?;
        let status = open(&operation.native_name, ServiceAccess::QUERY_STATUS)?
            .query_status()
            .map_err(|e| e.to_string())?;
        Ok(ServiceObservation {
            configuration,
            runtime: runtime(status.current_state),
            manager_generation: manager_generation()?,
            execution_generation: execution_generation(status.process_id)?,
        })
    }

    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        desired: ServiceTerminalState,
        expected: &ServiceObservation,
    ) -> Result<(), String> {
        operation.validate()?;
        let current = self.observe(operation)?;
        if &current != expected
            || current.configuration != operation.configuration
            || current.manager_generation != operation.manager_generation
        {
            return Err("service changed immediately before SCM control".into());
        }
        let service = open(
            &operation.native_name,
            ServiceAccess::QUERY_STATUS
                | match desired {
                    ServiceTerminalState::Stopped => ServiceAccess::STOP,
                    ServiceTerminalState::Running => ServiceAccess::START,
                },
        )?;
        let status = service.query_status().map_err(|e| e.to_string())?;
        if runtime(status.current_state) != expected.runtime
            || execution_generation(status.process_id)? != expected.execution_generation
        {
            return Err("service execution changed immediately before SCM control".into());
        }
        if desired.matches(runtime(status.current_state)) {
            return Ok(());
        }
        match desired {
            ServiceTerminalState::Stopped => {
                service.stop().map_err(|e| e.to_string())?;
            }
            ServiceTerminalState::Running => {
                service.start::<&str>(&[]).map_err(|e| e.to_string())?;
            }
        }
        let deadline = Instant::now() + operation.timeout_for(desired);
        let mut progress = Instant::now();
        let mut checkpoint = status.checkpoint;
        loop {
            let status = service.query_status().map_err(|e| e.to_string())?;
            if desired.matches(runtime(status.current_state)) {
                return Ok(());
            }
            if !matches!(
                status.current_state,
                ServiceState::StartPending | ServiceState::StopPending
            ) {
                return Err(format!(
                    "service `{}` reached {:?}, exit {:?}",
                    operation.native_name, status.current_state, status.exit_code
                ));
            }
            if status.checkpoint != checkpoint {
                checkpoint = status.checkpoint;
                progress = Instant::now();
            }
            let now = Instant::now();
            if now >= deadline
                || now.duration_since(progress) > status.wait_hint.max(Duration::from_secs(1))
            {
                return Err(format!(
                    "service `{}` transition timed out",
                    operation.native_name
                ));
            }
            std::thread::park_timeout(
                (status.wait_hint / 10)
                    .clamp(Duration::from_millis(50), Duration::from_secs(1))
                    .min(deadline - now),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scm_generation_uses_a_stable_native_boot_identity() {
        let first = super::manager_generation().expect("native boot can be identified");
        assert_eq!(super::manager_generation().unwrap(), first);
        assert_ne!(first, "stopped");
    }

    #[cfg(feature = "test-launcher")]
    mod native {
        use super::super::*;
        use std::path::PathBuf;
        use zup_transaction::{ServiceControlStage, apply_service_control, verify_service_control};

        struct Fixture {
            name: String,
            directory: PathBuf,
            configuration: zup_plan::ServiceState,
            service: Option<windows_service::service::Service>,
        }

        impl Fixture {
            fn new(mode: &str, account: zup_core::WindowsServiceAccount) -> Self {
                use zup_plan::InstallLocationResolver;
                let fixture_executable = std::env::var_os("ZUP_TEST_SCM_SERVICE_EXE")
                    .expect("run scripts/verify-windows-services.ps1");
                let target = zup_core::Target::host().expect("Windows target");
                let root = crate::host_dirs::WindowsInstallLocationResolver
                    .resolve(
                        zup_core::InstallLocation::Programs,
                        zup_core::SelectedScope::Machine,
                        &target,
                    )
                    .unwrap();
                let name = format!("ZupNativeTest-{}", uuid::Uuid::new_v4());
                let directory = crate::lowering::host_path(&root).join(&name);
                std::fs::create_dir(&directory).expect("elevated Program Files directory");
                let executable = directory.join("service.exe");
                let configuration = zup_plan::ServiceState::Registration {
                    execution: zup_core::ServiceExecution::Windows { account },
                    readiness: zup_core::ServiceReadiness::Manager,
                    display_name: name.clone(),
                    command: zup_plan::CommandSpec::new(
                        zup_plan::TargetPath::new(target, crate::plain_path_text(&executable))
                            .unwrap(),
                        vec![name.clone(), mode.into()],
                    ),
                    start: zup_core::ServiceStart::Manual,
                };
                let mut fixture = Self {
                    name: name.clone(),
                    directory,
                    configuration,
                    service: None,
                };
                std::fs::copy(fixture_executable, &executable).unwrap();
                let zup_plan::ServiceState::Registration {
                    execution,
                    display_name,
                    command,
                    start,
                    readiness,
                } = fixture.configuration.clone()
                else {
                    unreachable!()
                };
                crate::services::apply(&zup_plan::ServiceOperation {
                    execution,
                    display_name,
                    command,
                    start,
                    readiness,
                    key: zup_core::ResourceKey::Service {
                        id: zup_core::ServiceId::new(&fixture.name).unwrap(),
                    },
                    id: fixture.name.clone(),
                    name: fixture.name.clone(),
                    kind: zup_plan::IntegrationChangeKind::Create,
                    lifecycle: Default::default(),
                    requires_files: vec![],
                    privilege: zup_core::Privilege::System,
                    previous: zup_plan::ObservedServiceState::Absent,
                    conflict: None,
                })
                .expect("native owned service registration");
                fixture.service = Some(
                    open(
                        &fixture.name,
                        ServiceAccess::QUERY_STATUS
                            | ServiceAccess::QUERY_CONFIG
                            | ServiceAccess::STOP
                            | ServiceAccess::DELETE,
                    )
                    .unwrap(),
                );
                fixture
            }

            fn start(&self, timeout_seconds: u32) -> ServiceControlOperation {
                ServiceControlOperation {
                    key: zup_core::ResourceKey::Service {
                        id: zup_core::ServiceId::new(&self.name).unwrap(),
                    },
                    native_name: self.name.clone(),
                    privilege: zup_core::Privilege::System,
                    stage: ServiceControlStage::Restore,
                    configuration: self.configuration.clone(),
                    owned_configuration: self.configuration.clone(),
                    before: ServiceTerminalState::Stopped,
                    after: ServiceTerminalState::Running,
                    manager_generation: manager_generation().unwrap(),
                    before_execution_generation: "stopped".into(),
                    timeout_seconds,
                    rollback_timeout_seconds: 5,
                    dependencies: vec![],
                }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Some(service) = &self.service {
                    let target = zup_core::Target::host().unwrap();
                    if crate::scm::query_service(&self.name, &target)
                        .ok()
                        .and_then(|state| state.configuration())
                        .as_ref()
                        != Some(&self.configuration)
                    {
                        eprintln!(
                            "SCM fixture changed externally; cleanup refused: {}",
                            self.name
                        );
                        return;
                    }
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        match service.query_status().map(|status| status.current_state) {
                            Ok(ServiceState::Stopped) => break,
                            Ok(ServiceState::Running) => {
                                let _ = service.stop();
                            }
                            Err(error) => {
                                eprintln!("SCM cleanup observation failed: {error}");
                                return;
                            }
                            _ => {}
                        }
                        if Instant::now() >= deadline {
                            eprintln!("SCM fixture requires cleanup: {}", self.name);
                            return;
                        }
                        std::thread::park_timeout(Duration::from_millis(50));
                    }
                    if service.delete().is_err() {
                        return;
                    }
                }
                let _ = std::fs::remove_file(self.directory.join("service.exe"));
                let _ = std::fs::remove_dir(&self.directory);
            }
        }

        #[rstest::rstest]
        #[case(zup_core::WindowsServiceAccount::LocalService)]
        #[case(zup_core::WindowsServiceAccount::NetworkService)]
        #[case(zup_core::WindowsServiceAccount::LocalSystem)]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn explicit_accounts_pending_transitions_and_executable_locks_are_verified(
            #[case] account: zup_core::WindowsServiceAccount,
        ) {
            let fixture = Fixture::new("normal", account);
            let start = fixture.start(5);
            let receipt = apply_service_control(&mut ScmLifecycle, &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("service receipt");
            };
            verify_service_control(&mut ScmLifecycle, &receipt).unwrap();
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(fixture.directory.join("service.exe"))
                    .is_err()
            );
            let mut stop = start;
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = receipt.execution_generation.clone();
            let stopped = apply_service_control(&mut ScmLifecycle, &stop).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(stopped) = stopped else {
                panic!("service receipt");
            };
            verify_service_control(&mut ScmLifecycle, &stopped).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(fixture.directory.join("service.exe"))
                .unwrap();
            apply_service_control(&mut ScmLifecycle, &fixture.start(5)).unwrap();
        }

        #[test]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn failed_starts_and_pending_deadlines_never_produce_success_receipts() {
            let failed = Fixture::new("fail", zup_core::WindowsServiceAccount::LocalService);
            assert!(apply_service_control(&mut ScmLifecycle, &failed.start(5)).is_err());
            let slow = Fixture::new("slow", zup_core::WindowsServiceAccount::LocalService);
            assert!(apply_service_control(&mut ScmLifecycle, &slow.start(1)).is_err());
        }
    }
}
