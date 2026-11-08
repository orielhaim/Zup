use std::time::{Duration, Instant};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_transaction::{
    NativeServiceManager, ServiceControlOperation, ServiceObservation, ServiceTerminalState,
};

pub struct ScmLifecycle;

pub fn manager_generation() -> Result<String, String> {
    if !crate::is_process_elevated().map_err(|error| error.to_string())? {
        return Err("SCM generation observation requires machine authority".into());
    }
    use windows::Win32::{
        Foundation::ERROR_NO_MORE_FILES,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            RemoteDesktop::ProcessIdToSessionId,
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                IsProcessCritical, OpenProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
    };
    let failed =
        |error: windows::core::Error| format!("SCM process identity is unavailable: {error}");
    // SAFETY: Owned closes both snapshot and process handles; all output buffers
    // remain allocated for their documented Win32 calls.
    unsafe {
        let mut system = vec![0u16; 32768];
        let length = GetSystemDirectoryW(Some(&mut system)) as usize;
        if length == 0 || length >= system.len() {
            return Err("native system directory is unavailable".into());
        }
        let executable = format!(
            "{}\\services.exe",
            String::from_utf16(&system[..length]).map_err(|_| "invalid system directory")?
        );
        let snapshot = windows::core::Owned::new(
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(failed)?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        Process32FirstW(*snapshot, &mut entry).map_err(failed)?;
        let mut identity = None;
        loop {
            let length = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length])
                .eq_ignore_ascii_case("services.exe")
            {
                let handle = windows::core::Owned::new(
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION,
                        false,
                        entry.th32ProcessID,
                    )
                    .map_err(failed)?,
                );
                let mut path = vec![0u16; 32768];
                let mut length = path.len() as u32;
                QueryFullProcessImageNameW(
                    *handle,
                    PROCESS_NAME_WIN32,
                    windows::core::PWSTR(path.as_mut_ptr()),
                    &mut length,
                )
                .map_err(failed)?;
                let path = String::from_utf16(&path[..length as usize])
                    .map_err(|_| "invalid SCM executable path")?;
                if path.eq_ignore_ascii_case(&executable) {
                    let mut critical = windows::core::BOOL::default();
                    let mut session = u32::MAX;
                    IsProcessCritical(*handle, &mut critical).map_err(failed)?;
                    ProcessIdToSessionId(entry.th32ProcessID, &mut session).map_err(failed)?;
                    if !critical.as_bool() || session != 0 || identity.is_some() {
                        return Err("SCM process identity is ambiguous".into());
                    }
                    identity = Some(process_generation(entry.th32ProcessID, *handle)?);
                }
            }
            match Process32NextW(*snapshot, &mut entry) {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
                Err(error) => return Err(failed(error)),
            }
        }
        identity.ok_or_else(|| "native SCM process was not found".into())
    }
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

#[derive(Debug, thiserror::Error)]
enum GenerationError {
    #[error("SCM process exited during observation")]
    Exited,
    #[error("{0}")]
    Unavailable(String),
}

fn execution_generation(pid: u32) -> Result<String, GenerationError> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: Query-only access; Owned closes the process handle.
    let handle = unsafe {
        windows::core::Owned::new(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(|error| {
                if error.code()
                    == windows::core::HRESULT::from_win32(
                        windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0,
                    )
                {
                    GenerationError::Exited
                } else {
                    GenerationError::Unavailable(format!("{error}: {error:?}"))
                }
            })?,
        )
    };
    process_generation(pid, *handle).map_err(GenerationError::Unavailable)
}

fn process_generation(
    pid: u32,
    handle: windows::Win32::Foundation::HANDLE,
) -> Result<String, String> {
    use windows::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: The caller retains a query-capable handle; outputs are initialized.
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|e| format!("{e}: {e:?}"))?;
    Ok(format!(
        "{pid}:{}:{}",
        creation.dwHighDateTime, creation.dwLowDateTime
    ))
}

fn status_generation(status: &windows_service::service::ServiceStatus) -> Result<String, String> {
    status_generation_snapshot(status).map_err(|error| error.to_string())
}

fn status_generation_snapshot(
    status: &windows_service::service::ServiceStatus,
) -> Result<String, GenerationError> {
    match status.current_state {
        ServiceState::Stopped => Ok("stopped".into()),
        ServiceState::StartPending | ServiceState::StopPending => Ok("pending".into()),
        _ => execution_generation(status.process_id.filter(|pid| *pid != 0).ok_or_else(|| {
            GenerationError::Unavailable(
                "SCM did not provide a valid running process identity".into(),
            )
        })?),
    }
}

fn service_processes_exited(operation: &ServiceControlOperation) -> Result<bool, String> {
    let zup_plan::ServiceState::Registration { command, .. } = &operation.configuration else {
        return Err("service process verification requires registration".into());
    };
    let executable = crate::lowering::host_path(&command.executable);
    if crate::restart_manager::preflight(&[&executable])?
        != crate::restart_manager::FilePreflight::Ready
    {
        return Ok(false);
    }
    if operation.before_execution_generation == "stopped" {
        return Ok(true);
    }
    let parts = operation
        .before_execution_generation
        .split(':')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid authorized SCM process generation")?;
    let [pid, high, low] = parts.as_slice() else {
        return Err("invalid authorized SCM process generation".into());
    };
    let pid = *pid;
    if pid == 0 || format!("{pid}:{high}:{low}") != operation.before_execution_generation {
        return Err("invalid authorized SCM process generation".into());
    }
    use windows::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };
    // SAFETY: The query handle binds creation time and exit state to one process object.
    unsafe {
        let handle = match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(handle) => windows::core::Owned::new(handle),
            Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(true),
            Err(error) => return Err(error.to_string()),
        };
        if process_generation(pid, *handle)? != operation.before_execution_generation {
            return Ok(true);
        }
        match WaitForSingleObject(*handle, 0) {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err("SCM process exit could not be established".into()),
        }
    }
}

impl NativeServiceManager for ScmLifecycle {
    fn finish_startup_inhibition(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<(), String> {
        if operation.before_configuration == operation.configuration {
            return Ok(());
        }
        if !matches!(
            operation.configuration,
            zup_plan::ServiceState::Registration {
                start: zup_core::ServiceStart::Disabled,
                ..
            }
        ) {
            return Err("SCM inhibition must leave startup disabled".into());
        }
        if !self.stopped_runtime_is_quiescent(operation, &operation.configuration)? {
            return Err("SCM startup inhibition or stopped state changed".into());
        }
        Ok(())
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        if !matches!(
            operation.before_configuration,
            zup_plan::ServiceState::Registration {
                start: zup_core::ServiceStart::Disabled,
                ..
            }
        ) && !self.stopped_runtime_is_quiescent(operation, &operation.configuration)?
        {
            return Ok(false);
        }
        let status = open(&operation.native_name, ServiceAccess::QUERY_STATUS)?
            .query_status()
            .map_err(|error| format!("{error}: {error:?}"))?;
        Ok(status.current_state == ServiceState::Stopped
            && service_processes_exited(operation)?
            && match status.exit_code {
                windows_service::service::ServiceExitCode::Win32(code) => code != 0,
                windows_service::service::ServiceExitCode::ServiceSpecific(_) => true,
            })
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        operation: &ServiceControlOperation,
        configuration: &zup_plan::ServiceState,
    ) -> Result<bool, String> {
        if !matches!(
            configuration,
            zup_plan::ServiceState::Registration {
                failure: zup_core::ServiceFailurePolicy::None,
                ..
            } | zup_plan::ServiceState::Registration {
                start: zup_core::ServiceStart::Disabled,
                ..
            }
        ) {
            return Ok(false);
        }
        let current = self.observe(operation)?;
        Ok(&current.configuration == configuration
            && current.manager_generation == operation.manager_generation
            && current.runtime == zup_plan::ServiceRuntimeState::Stopped
            && service_processes_exited(operation)?)
    }
    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String> {
        let target = match &operation.configuration {
            zup_plan::ServiceState::Registration { command, .. } => command.executable.target(),
            _ => return Err("service control requires registration".into()),
        };
        let registration = crate::scm::query_service(&operation.native_name, target)
            .map_err(|e| format!("{e}: {e:?}"))?;
        let configuration = registration
            .configuration()
            .ok_or("unrecognized SCM registration")?;
        let service = open(&operation.native_name, ServiceAccess::QUERY_STATUS)?;
        let deadline = Instant::now() + operation.timeout_for(operation.after);
        loop {
            let status = service.query_status().map_err(|error| error.to_string())?;
            match status_generation_snapshot(&status) {
                Ok(execution_generation) => {
                    let checked = service.query_status().map_err(|error| error.to_string())?;
                    if checked.current_state == status.current_state
                        && checked.process_id == status.process_id
                    {
                        return Ok(ServiceObservation {
                            configuration,
                            runtime: runtime(status.current_state),
                            manager_generation: manager_generation()?,
                            execution_generation,
                        });
                    }
                }
                Err(GenerationError::Exited) => {}
                Err(error) => return Err(error.to_string()),
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("SCM process did not stabilize during identity observation".into());
            }
            std::thread::park_timeout(Duration::from_millis(20).min(deadline - now));
        }
    }

    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        desired: ServiceTerminalState,
        expected: &ServiceObservation,
        deadline: Instant,
    ) -> Result<ServiceObservation, String> {
        let within_deadline = || {
            if Instant::now() < deadline {
                Ok(())
            } else {
                Err(format!(
                    "service `{}` transition timed out",
                    operation.native_name
                ))
            }
        };
        operation.validate()?;
        let current = self.observe(operation)?;
        if &current != expected
            || current.configuration != operation.before_configuration
            || current.manager_generation != operation.manager_generation
        {
            return Err("service changed immediately before SCM control".into());
        }
        let process = if desired == ServiceTerminalState::Stopped
            && expected.runtime == zup_plan::ServiceRuntimeState::Running
        {
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            };
            let status = open(&operation.native_name, ServiceAccess::QUERY_STATUS)?
                .query_status()
                .map_err(|error| error.to_string())?;
            let pid = status
                .process_id
                .ok_or("running service process identity is unavailable")?;
            // SAFETY: Owned retains a query-capable process handle until shutdown verification completes.
            let handle = unsafe {
                windows::core::Owned::new(
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                        false,
                        pid,
                    )
                    .map_err(|error| error.to_string())?,
                )
            };
            if status.current_state != ServiceState::Running
                || process_generation(pid, *handle)? != expected.execution_generation
            {
                return Err("service process changed before shutdown".into());
            }
            Some(handle)
        } else {
            None
        };
        let settled = || {
            within_deadline()?;
            if let Some(process) = &process {
                use windows::Win32::{
                    Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
                };
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("service process exit deadline expired")?;
                let milliseconds = u32::try_from(remaining.as_millis().max(1))
                    .map_err(|_| "invalid process wait deadline")?;
                // SAFETY: The retained handle has synchronization access and the wait is bounded.
                if unsafe { WaitForSingleObject(**process, milliseconds) } != WAIT_OBJECT_0 {
                    return Err(
                        "service reported Stopped but its process did not exit before the deadline"
                            .into(),
                    );
                }
            }
            within_deadline()
        };
        if operation.before_configuration != operation.configuration {
            if desired == ServiceTerminalState::Stopped
                && !matches!(
                    operation.configuration,
                    zup_plan::ServiceState::Registration {
                        start: zup_core::ServiceStart::Disabled,
                        ..
                    }
                )
            {
                require_safe_stopped_startup_restore(
                    &operation.native_name,
                    &operation.owned_configuration,
                )?;
            }
            within_deadline()?;
            crate::services::change_live_configuration(
                &operation.native_name,
                &operation.before_configuration,
                &operation.configuration,
            )
            .map_err(|error| error.to_string())?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::NativeConfigurationChanged,
                &operation.native_name,
            );
        }
        let service = open(
            &operation.native_name,
            ServiceAccess::QUERY_STATUS
                | match desired {
                    ServiceTerminalState::Stopped => ServiceAccess::STOP,
                    ServiceTerminalState::Running => ServiceAccess::START,
                },
        )?;
        let status = service.query_status().map_err(|e| format!("{e}: {e:?}"))?;
        if operation.before_configuration != operation.configuration
            && desired == ServiceTerminalState::Running
            && status.current_state == ServiceState::Running
        {
            let released = self.observe(operation)?;
            if released.configuration == operation.configuration
                && released.manager_generation == operation.manager_generation
                && released.runtime == zup_plan::ServiceRuntimeState::Running
            {
                within_deadline()?;
                return Ok(released);
            }
            return Err("service changed after restoring startup configuration".into());
        }
        if runtime(status.current_state) != expected.runtime
            || status_generation(&status)? != expected.execution_generation
        {
            return Err("service execution changed immediately before SCM control".into());
        }
        if desired.matches(runtime(status.current_state)) {
            settled()?;
            return self.observe(operation);
        }
        within_deadline()?;
        match desired {
            ServiceTerminalState::Stopped => {
                service.stop().map_err(|e| format!("{e}: {e:?}"))?;
            }
            ServiceTerminalState::Running => {
                if let Err(error) = service.start::<&str>(&[]) {
                    let already_running = matches!(&error, windows_service::Error::Winapi(error) if error.raw_os_error() == Some(1056));
                    if !already_running || operation.before_configuration == operation.configuration
                    {
                        return Err(format!("{error}: {error:?}"));
                    }
                }
            }
        }
        zup_transaction::fault_injection::boundary(
            zup_transaction::fault_injection::Boundary::NativeRequestSubmitted,
            &operation.native_name,
        );
        let mut progress = Instant::now();
        let mut checkpoint = status.checkpoint;
        let mut pending_seen = false;
        loop {
            let status = service.query_status().map_err(|e| format!("{e}: {e:?}"))?;
            within_deadline()?;
            if desired.matches(runtime(status.current_state)) {
                settled()?;
                return self.observe(operation);
            }
            let pending = matches!(
                status.current_state,
                ServiceState::StartPending | ServiceState::StopPending
            );
            pending_seen |= pending;
            let awaiting_stop_dispatch = desired == ServiceTerminalState::Stopped
                && status.current_state == ServiceState::Running
                && !pending_seen;
            if awaiting_stop_dispatch
                && status_generation(&status)? != expected.execution_generation
            {
                return Err("service execution changed while awaiting shutdown dispatch".into());
            }
            if !pending && !awaiting_stop_dispatch {
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
                || (!awaiting_stop_dispatch
                    && now.duration_since(progress) > status.wait_hint.max(Duration::from_secs(1)))
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

pub(crate) fn require_safe_stopped_startup_restore(
    name: &str,
    previous: &zup_plan::ServiceState,
) -> Result<(), String> {
    let status = open(name, ServiceAccess::QUERY_STATUS)?
        .query_status()
        .map_err(|error| error.to_string())?;
    if status.current_state == ServiceState::Stopped
        && !matches!(
            previous,
            zup_plan::ServiceState::Registration {
                failure: zup_core::ServiceFailurePolicy::Windows { .. },
                ..
            }
        )
    {
        return Ok(());
    }
    Err("SCM does not expose pending recovery actions; an existing service with a recovery policy cannot safely be reenabled while preserving Stopped. Startup remains disabled; explicit administrator reconciliation is required".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn scm_generation_requires_machine_authority_and_identifies_the_native_manager() {
        if !crate::is_process_elevated().unwrap() {
            assert_eq!(
                super::manager_generation().unwrap_err(),
                "SCM generation observation requires machine authority"
            );
            return;
        }
        let first = super::manager_generation().expect("native manager can be identified");
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
            fn new(
                mode: &str,
                account: zup_core::WindowsServiceAccount,
                failure: zup_core::ServiceFailurePolicy,
                dependencies: Vec<String>,
            ) -> Self {
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
                    native_timeouts: zup_core::NativeServiceTimeouts::Scm,
                    dependencies,
                    failure,
                    execution: zup_core::ServiceExecution::Windows { account },
                    readiness: zup_core::ServiceReadiness::Manager,
                    display_name: name.clone(),
                    command: Box::new(zup_plan::CommandSpec::new(
                        zup_plan::TargetPath::new(target, crate::plain_path_text(&executable))
                            .unwrap(),
                        vec![name.clone(), mode.into()],
                    )),
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
                    failure,
                    dependencies,
                    execution,
                    display_name,
                    command,
                    start,
                    readiness,
                    ..
                } = fixture.configuration.clone()
                else {
                    unreachable!()
                };
                crate::services::apply(
                    &zup_plan::ServiceOperation {
                        failure,
                        execution,
                        display_name,
                        command: *command,
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
                        requires_services: Vec::new(),
                        dependency_names: dependencies,
                        privilege: zup_core::Privilege::System,
                        previous: zup_plan::ObservedServiceState::Absent,
                        conflict: None,
                    },
                    false,
                )
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
                    intent: zup_transaction::ServiceControlIntent::Transition,
                    key: zup_core::ResourceKey::Service {
                        id: zup_core::ServiceId::new(&self.name).unwrap(),
                    },
                    native_name: self.name.clone(),
                    privilege: zup_core::Privilege::System,
                    stage: ServiceControlStage::Restore,
                    before_configuration: self.configuration.clone(),
                    configuration: self.configuration.clone(),
                    owned_configuration: self.configuration.clone(),
                    before: ServiceTerminalState::Stopped,
                    after: ServiceTerminalState::Running,
                    manager_generation: manager_generation().unwrap(),
                    before_execution_generation: "stopped".into(),
                    timeouts: zup_core::ServiceTimeouts::Windows {
                        start_observation_seconds: timeout_seconds,
                        stop_observation_seconds: 5,
                    },
                    dependencies: vec![],
                }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Some(service) = &self.service {
                    let target = zup_core::Target::host().unwrap();
                    let current = crate::scm::query_service(&self.name, &target)
                        .ok()
                        .and_then(|state| state.configuration());
                    if current.as_ref() != Some(&self.configuration)
                        && current != zup_transaction::disabled_startup(&self.configuration)
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
            let fixture = Fixture::new("normal", account, Default::default(), Vec::new());
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
        fn native_failure_actions_stop_after_the_declared_restart_budget() {
            let fixture = Fixture::new(
                "crash",
                zup_core::WindowsServiceAccount::LocalService,
                zup_core::ServiceFailurePolicy::Windows {
                    delay_seconds: 1,
                    restart_attempts: 2,
                    reset_seconds: 60,
                },
                Vec::new(),
            );
            let start = fixture.start(5);
            let receipt = apply_service_control(&mut ScmLifecycle, &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("native service receipt")
            };
            let mut generations =
                std::collections::BTreeSet::from([receipt.execution_generation.clone()]);
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stopped_since = None;
            loop {
                let observation = ScmLifecycle.observe(&start).unwrap();
                if observation.runtime == zup_plan::ServiceRuntimeState::Running {
                    generations.insert(observation.execution_generation);
                    stopped_since = None;
                } else if observation.runtime == zup_plan::ServiceRuntimeState::Stopped
                    && generations.len() == 3
                {
                    let stopped = stopped_since.get_or_insert_with(Instant::now);
                    if stopped.elapsed() >= Duration::from_secs(2) {
                        break;
                    }
                }
                assert!(
                    generations.len() <= 3,
                    "SCM exceeded its declared restart budget"
                );
                assert!(
                    Instant::now() < deadline,
                    "SCM did not perform two bounded native restarts"
                );
                std::thread::park_timeout(Duration::from_millis(20));
            }
            assert_eq!(generations.len(), 3);
        }

        #[test]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn native_dependencies_refuse_stopping_a_required_running_service() {
            let dependency = Fixture::new(
                "normal",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let dependent = Fixture::new(
                "normal",
                zup_core::WindowsServiceAccount::NetworkService,
                Default::default(),
                vec![dependency.name.to_lowercase()],
            );
            let start = dependency.start(5);
            let receipt = apply_service_control(&mut ScmLifecycle, &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("native service receipt")
            };
            apply_service_control(&mut ScmLifecycle, &dependent.start(5)).unwrap();
            let mut stop = start;
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = receipt.execution_generation.clone();
            assert!(apply_service_control(&mut ScmLifecycle, &stop).is_err());
            let current = ScmLifecycle.observe(&stop).unwrap();
            assert_eq!(current.runtime, zup_plan::ServiceRuntimeState::Running);
            assert_eq!(current.execution_generation, receipt.execution_generation);
            drop(dependent);
            apply_service_control(&mut ScmLifecycle, &stop).unwrap();
        }

        #[rstest::rstest]
        #[case::running(false, false, false)]
        #[case::stopped_without_execution(false, true, false)]
        #[case::queued_restart(true, true, false)]
        #[case::application_reports_never_started(false, true, true)]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn startup_inhibition_protects_payloads_and_refuses_ambiguous_release(
            #[case] queued: bool,
            #[case] initially_stopped: bool,
            #[case] reports_never_started: bool,
        ) {
            let fixture = Fixture::new(
                if reports_never_started {
                    "reported-never-started"
                } else if queued {
                    "crash"
                } else {
                    "normal"
                },
                zup_core::WindowsServiceAccount::LocalService,
                zup_core::ServiceFailurePolicy::Windows {
                    delay_seconds: 2,
                    restart_attempts: 2,
                    reset_seconds: 60,
                },
                Vec::new(),
            );
            let start = fixture.start(8);
            if reports_never_started {
                assert!(apply_service_control(&mut ScmLifecycle, &start).is_err());
                assert_eq!(
                    fixture
                        .service
                        .as_ref()
                        .unwrap()
                        .query_status()
                        .unwrap()
                        .exit_code,
                    windows_service::service::ServiceExitCode::Win32(1077)
                );
            } else if !initially_stopped || queued {
                apply_service_control(&mut ScmLifecycle, &start).unwrap();
            }
            if queued {
                let deadline = Instant::now() + Duration::from_secs(3);
                while ScmLifecycle.observe(&start).unwrap().runtime
                    != zup_plan::ServiceRuntimeState::Stopped
                {
                    assert!(Instant::now() < deadline);
                    std::thread::park_timeout(Duration::from_millis(20));
                }
            }
            let before = ScmLifecycle.observe(&start).unwrap();
            let mut stop = start.clone();
            stop.stage = ServiceControlStage::Quiesce;
            stop.configuration =
                zup_transaction::disabled_startup(&stop.before_configuration).unwrap();
            stop.before = if initially_stopped {
                ServiceTerminalState::Stopped
            } else {
                ServiceTerminalState::Running
            };
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = before.execution_generation;
            let zup_transaction::OperationReceipt::ServiceControl(stopped) =
                apply_service_control(&mut ScmLifecycle, &stop).unwrap()
            else {
                panic!("shutdown receipt")
            };
            verify_service_control(&mut ScmLifecycle, &stopped).unwrap();
            assert!(
                ScmLifecycle
                    .stopped_runtime_is_quiescent(&stop, &stop.configuration)
                    .unwrap()
            );
            let mut release = start.clone();
            release.before_configuration = stop.configuration.clone();
            release.after = if initially_stopped {
                ServiceTerminalState::Stopped
            } else {
                ServiceTerminalState::Running
            };
            if initially_stopped {
                assert!(
                    apply_service_control(&mut ScmLifecycle, &release)
                        .unwrap_err()
                        .contains("pending recovery actions")
                );
                assert!(
                    ScmLifecycle
                        .stopped_runtime_is_quiescent(&stop, &stop.configuration)
                        .unwrap()
                );
                return;
            }
            let zup_transaction::OperationReceipt::ServiceControl(released) =
                apply_service_control(&mut ScmLifecycle, &release).unwrap()
            else {
                panic!("restoration receipt")
            };
            verify_service_control(&mut ScmLifecycle, &released).unwrap();
            zup_transaction::rollback_service_control(&mut ScmLifecycle, &released).unwrap();
            zup_transaction::rollback_service_control(&mut ScmLifecycle, &stopped).unwrap();
            let restored = ScmLifecycle.observe(&start).unwrap();
            assert_eq!(restored.configuration, fixture.configuration);
            assert_eq!(
                restored.runtime,
                if initially_stopped {
                    zup_plan::ServiceRuntimeState::Stopped
                } else {
                    zup_plan::ServiceRuntimeState::Running
                }
            );
        }

        #[test]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn stopped_receipts_wait_for_the_native_process_to_exit() {
            use windows::Win32::{
                Foundation::WAIT_OBJECT_0,
                System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
            };
            let fixture = Fixture::new(
                "linger-stop",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let mut stop = fixture.start(5);
            let zup_transaction::OperationReceipt::ServiceControl(started) =
                apply_service_control(&mut ScmLifecycle, &stop).unwrap()
            else {
                unreachable!()
            };
            let pid = open(&fixture.name, ServiceAccess::QUERY_STATUS)
                .unwrap()
                .query_status()
                .unwrap()
                .process_id
                .unwrap();
            // SAFETY: Owned retains a synchronization handle to the service process.
            let process = unsafe {
                windows::core::Owned::new(OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap())
            };
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = started.execution_generation;
            let receipt = apply_service_control(&mut ScmLifecycle, &stop).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(stopped) = receipt else {
                unreachable!()
            };
            verify_service_control(&mut ScmLifecycle, &stopped).unwrap();
            // SAFETY: The retained process handle has synchronization access.
            assert_eq!(unsafe { WaitForSingleObject(*process, 0) }, WAIT_OBJECT_0);
        }

        #[test]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn lost_stop_receipts_do_not_release_a_process_that_reported_stopped() {
            use windows::Win32::{
                Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
                System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
            };
            struct Release(std::path::PathBuf);
            impl Drop for Release {
                fn drop(&mut self) {
                    let _ = std::fs::write(&self.0, b"exit");
                }
            }
            let fixture = Fixture::new(
                "linger-stop-gated",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let release = Release(fixture.directory.join("service.exit"));
            let mut operation = fixture.start(5);
            let zup_transaction::OperationReceipt::ServiceControl(started) =
                apply_service_control(&mut ScmLifecycle, &operation).unwrap()
            else {
                unreachable!()
            };
            let pid = fixture
                .service
                .as_ref()
                .unwrap()
                .query_status()
                .unwrap()
                .process_id
                .unwrap();
            // SAFETY: Owned keeps the original process object available after SCM clears its PID.
            let process = unsafe {
                windows::core::Owned::new(OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap())
            };
            operation.stage = ServiceControlStage::Quiesce;
            operation.before = ServiceTerminalState::Running;
            operation.after = ServiceTerminalState::Stopped;
            operation.before_execution_generation = started.execution_generation;
            operation.configuration =
                zup_transaction::disabled_startup(&fixture.configuration).unwrap();
            crate::services::change_live_configuration(
                &fixture.name,
                &fixture.configuration,
                &operation.configuration,
            )
            .unwrap();
            fixture.service.as_ref().unwrap().stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while fixture
                .service
                .as_ref()
                .unwrap()
                .query_status()
                .unwrap()
                .current_state
                != ServiceState::Stopped
            {
                assert!(Instant::now() < deadline, "fixture did not report Stopped");
                std::thread::park_timeout(Duration::from_millis(20));
            }
            // SAFETY: The retained handle has synchronization access.
            assert_eq!(unsafe { WaitForSingleObject(*process, 0) }, WAIT_TIMEOUT);
            assert!(
                !ScmLifecycle
                    .stopped_runtime_is_quiescent(&operation, &operation.configuration)
                    .unwrap()
            );
            assert!(matches!(
                zup_transaction::reconcile_service_control(&mut ScmLifecycle, &operation, None),
                Err(_) | Ok(zup_transaction::ReconcileResult::Ambiguous)
            ));
            drop(release);
            // SAFETY: Wait only observes the retained process; it cannot terminate it.
            assert_eq!(
                unsafe { WaitForSingleObject(*process, 10000) },
                WAIT_OBJECT_0
            );
            assert!(matches!(
                zup_transaction::reconcile_service_control(&mut ScmLifecycle, &operation, None)
                    .unwrap(),
                zup_transaction::ReconcileResult::AppliedWithReceipt(_)
            ));
            std::fs::remove_file(fixture.directory.join("service.exit")).unwrap();
        }

        #[test]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn failed_starts_and_pending_deadlines_never_produce_success_receipts() {
            let failed = Fixture::new(
                "fail",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            assert!(apply_service_control(&mut ScmLifecycle, &failed.start(5)).is_err());
            let slow = Fixture::new(
                "slow",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            assert!(apply_service_control(&mut ScmLifecycle, &slow.start(1)).is_err());
            let slow_stop = Fixture::new(
                "slow-stop",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let mut stop = slow_stop.start(5);
            let receipt = apply_service_control(&mut ScmLifecycle, &stop).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                unreachable!()
            };
            let pid = open(&slow_stop.name, ServiceAccess::QUERY_STATUS)
                .unwrap()
                .query_status()
                .unwrap()
                .process_id
                .unwrap();
            use windows::Win32::{
                Foundation::WAIT_TIMEOUT,
                System::Threading::{
                    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                    WaitForSingleObject,
                },
            };
            // SAFETY: Owned retains the running process for query and wait operations.
            let process = unsafe {
                windows::core::Owned::new(
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                        false,
                        pid,
                    )
                    .unwrap(),
                )
            };
            assert_eq!(
                process_generation(pid, *process).unwrap(),
                receipt.execution_generation
            );
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = receipt.execution_generation.clone();
            stop.timeouts = zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 5,
                stop_observation_seconds: 1,
            };
            assert!(apply_service_control(&mut ScmLifecycle, &stop).is_err());
            let pending = ScmLifecycle.observe(&stop).unwrap();
            assert_eq!(pending.runtime, zup_plan::ServiceRuntimeState::StopPending);
            // SAFETY: The retained process handle has synchronization access.
            assert_eq!(unsafe { WaitForSingleObject(*process, 0) }, WAIT_TIMEOUT);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if ScmLifecycle.observe(&stop).unwrap().runtime
                    == zup_plan::ServiceRuntimeState::Stopped
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture did not finish its own shutdown"
                );
                std::thread::park_timeout(Duration::from_millis(20));
            }
        }

        struct NativeTransaction {
            files: crate::WindowsFileExecutor<zup_bundle::DirectoryPayloadSource>,
            fail_readiness: bool,
            abort_after_replacement: bool,
        }

        impl zup_transaction::OperationExecutor for NativeTransaction {
            type Error = String;
            fn prepare(&mut self, _: &zup_transaction::TransactionNode) -> Result<(), String> {
                Ok(())
            }
            fn apply(
                &mut self,
                node: &zup_transaction::TransactionNode,
            ) -> Result<zup_transaction::OperationReceipt, String> {
                use zup_transaction::{NodeKind, OperationReceipt};
                match &node.kind {
                    NodeKind::ServiceControl { operation } => {
                        apply_service_control(&mut ScmLifecycle, operation)
                    }
                    NodeKind::Barrier => Ok(OperationReceipt::Control),
                    NodeKind::StageFile {
                        key: zup_core::ResourceKey::File { destination },
                    }
                    | NodeKind::FileMutation {
                        key: zup_core::ResourceKey::File { destination },
                        ..
                    } => {
                        let path = zup_plan::TargetPath::new(
                            zup_core::Target::host().unwrap(),
                            destination,
                        )
                        .unwrap();
                        let receipt = crate::apply_node(
                            &mut self.files,
                            node,
                            node.meta.source_relative.as_ref().unwrap(),
                            &crate::lowering::host_path(&path),
                        )
                        .map(crate::transaction_receipt)
                        .map_err(|error| error.to_string())?;
                        if self.abort_after_replacement
                            && matches!(node.kind, NodeKind::FileMutation { .. })
                        {
                            std::process::abort();
                        }
                        Ok(receipt)
                    }
                    _ => Err("unexpected native transaction operation".into()),
                }
            }
            fn verify(
                &mut self,
                _: &zup_transaction::TransactionNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<(), String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        verify_service_control(&mut ScmLifecycle, receipt)?;
                        if self.fail_readiness
                            && receipt.operation.stage == ServiceControlStage::Restore
                        {
                            self.fail_readiness = false;
                            return Err("injected failure after native readiness".into());
                        }
                        Ok(())
                    }
                    zup_transaction::OperationReceipt::Control => Ok(()),
                    _ => crate::verify_installed_file(receipt).map_err(|error| error.to_string()),
                }
            }
            fn rollback(
                &mut self,
                _: &zup_transaction::TransactionNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<(), String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        zup_transaction::rollback_service_control(&mut ScmLifecycle, receipt)
                    }
                    zup_transaction::OperationReceipt::Control => Ok(()),
                    _ => self
                        .files
                        .rollback_transaction_receipt(receipt)
                        .map_err(|error| error.to_string()),
                }
            }
            fn reconcile(
                &mut self,
                node: &zup_transaction::TransactionNode,
                receipt: Option<&zup_transaction::OperationReceipt>,
            ) -> Result<zup_transaction::ReconcileResult, String> {
                if let zup_transaction::NodeKind::ServiceControl { operation } = &node.kind {
                    let receipt = match receipt {
                        Some(zup_transaction::OperationReceipt::ServiceControl(receipt)) => {
                            Some(receipt.as_ref())
                        }
                        _ => None,
                    };
                    return zup_transaction::reconcile_service_control(
                        &mut ScmLifecycle,
                        operation,
                        receipt,
                    );
                }
                self.files
                    .reconcile_transaction_node(node)
                    .map_err(|error| error.to_string())
            }
            fn reconcile_rollback(
                &mut self,
                _: &zup_transaction::TransactionNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<zup_transaction::RollbackReconcileResult, String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        zup_transaction::reconcile_service_rollback(&mut ScmLifecycle, receipt)
                    }
                    _ => Ok(zup_transaction::RollbackReconcileResult::Retry),
                }
            }
        }

        fn transaction_executor(
            record: &zup_transaction::TransactionRecord,
            root: &std::path::Path,
        ) -> NativeTransaction {
            let mut files = crate::WindowsFileExecutor::new(
                zup_bundle::DirectoryPayloadSource::new(root.join("payload")),
                root.join("work"),
                record.transaction_id.to_string(),
                ".exe",
                Box::new(crate::NullProgress),
            );
            for node in &record.plan.nodes {
                if let (Some(precondition), Some(hash), Some(size)) = (
                    node.meta.file_precondition,
                    node.meta.expected_sha256,
                    node.meta.expected_size,
                ) {
                    files.register_file_op(&node.id, precondition, hash, size);
                }
            }
            NativeTransaction {
                files,
                fail_readiness: false,
                abort_after_replacement: false,
            }
        }

        pub(super) fn transaction_child() {
            use zup_transaction::TransactionStore;
            let root = PathBuf::from(
                std::env::var_os("ZUP_SCM_TRANSACTION_ROOT")
                    .expect("the parent supplies a private fixture"),
            );
            let id = std::env::var("ZUP_SCM_TRANSACTION_ID")
                .unwrap()
                .parse()
                .unwrap();
            let store =
                zup_transaction::FilesystemTransactionStore::private_at(root.join("journals"));
            let record = store.load(&id).unwrap();
            let mut executor = transaction_executor(&record, &root);
            executor.abort_after_replacement = true;
            zup_transaction::TransactionCoordinator::new(store)
                .execute(record, &mut executor)
                .unwrap();
            panic!("the replacement crash did not fire");
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum TransactionScenario {
            RunningUpgrade,
            StoppedUpgrade,
            ReadinessFailure,
            FailedStartUpgrade,
            CrashAfterReplacement,
        }

        #[rstest::rstest]
        #[case(TransactionScenario::RunningUpgrade)]
        #[case(TransactionScenario::StoppedUpgrade)]
        #[case(TransactionScenario::ReadinessFailure)]
        #[case(TransactionScenario::FailedStartUpgrade)]
        #[case(TransactionScenario::CrashAfterReplacement)]
        #[ignore = "requires elevated isolated SCM; run scripts/verify-windows-services.ps1"]
        fn native_transactions_replace_compensate_and_recover_real_service_executables(
            #[case] scenario: TransactionScenario,
        ) {
            use zup_transaction::TransactionStore;
            let fixture = Fixture::new(
                "normal",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let mut input =
                zup_transaction::TransactionInput::new(zup_core::Target::host().unwrap());
            if scenario != TransactionScenario::StoppedUpgrade {
                let start = fixture.start(5);
                let receipt = apply_service_control(&mut ScmLifecycle, &start).unwrap();
                let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                    unreachable!()
                };
                let mut stop = start.clone();
                stop.stage = ServiceControlStage::Quiesce;
                stop.before = ServiceTerminalState::Running;
                stop.after = ServiceTerminalState::Stopped;
                stop.before_execution_generation = receipt.execution_generation;
                input.service_controls.extend([stop, start]);
            }
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("payload")).unwrap();
            let original = std::fs::read(fixture.directory.join("service.exe")).unwrap();
            let mut replacement = original.clone();
            replacement.extend_from_slice(if scenario == TransactionScenario::FailedStartUpgrade {
                b"zup-native-start-failure"
            } else {
                b"zup-native-upgrade-2"
            });
            std::fs::write(root.path().join("payload/service.exe"), &replacement).unwrap();
            let zup_plan::ServiceState::Registration { command, .. } = &fixture.configuration
            else {
                unreachable!()
            };
            input.files.push(zup_plan::FileOperation {
                key: zup_core::ResourceKey::File {
                    destination: command.executable.to_string(),
                },
                destination: command.executable.clone(),
                source_relative: zup_core::RelativePath::new("service.exe").unwrap(),
                precondition: zup_plan::FilePrecondition::Exact {
                    size: original.len() as u64,
                    sha256: zup_core::hash_bytes(&original),
                },
                expected_size: replacement.len() as u64,
                expected_sha256: zup_core::hash_bytes(&replacement),
                executable: true,
                kind: zup_plan::FileOperationKind::Replace,
                privilege: zup_core::Privilege::System,
                conflict: None,
            });
            let record = zup_transaction::TransactionRecord::new(
                zup_transaction::TransactionId::new_v7(),
                zup_core::AppId::new("com.example.scm-transaction").unwrap(),
                zup_core::SelectedScope::Machine,
                "1.0.0".parse().unwrap(),
                zup_transaction::compile_transaction(&input).unwrap(),
            );
            let store = zup_transaction::FilesystemTransactionStore::private_at(
                root.path().join("journals"),
            );
            store.create(&record).unwrap();
            let mut executor = transaction_executor(&record, root.path());
            executor.fail_readiness = scenario == TransactionScenario::ReadinessFailure;
            let (settled, outcome) = if scenario == TransactionScenario::CrashAfterReplacement {
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--ignored",
                        "--exact",
                        "service_control::tests::native_transaction_child",
                        "--nocapture",
                    ])
                    .env("ZUP_SCM_TRANSACTION_ROOT", root.path())
                    .env("ZUP_SCM_TRANSACTION_ID", record.transaction_id.to_string())
                    .status()
                    .unwrap();
                assert!(!status.success());
                assert_eq!(
                    ScmLifecycle.observe(&fixture.start(5)).unwrap().runtime,
                    zup_plan::ServiceRuntimeState::Stopped
                );
                zup_transaction::recover(
                    store.load(&record.transaction_id).unwrap(),
                    &store,
                    &mut executor,
                )
                .unwrap()
            } else {
                zup_transaction::TransactionCoordinator::new(store)
                    .execute(record, &mut executor)
                    .unwrap()
            };
            let rollback = matches!(
                scenario,
                TransactionScenario::ReadinessFailure | TransactionScenario::FailedStartUpgrade
            );
            assert_eq!(
                outcome,
                if rollback {
                    zup_transaction::TransactionOutcome::RolledBack
                } else {
                    zup_transaction::TransactionOutcome::Committed
                },
                "failure: {:?}; rollback: {:?}; recovery: {:?}",
                settled.failure,
                settled.rollback_failure,
                settled.recovery_failure
            );
            assert_eq!(
                std::fs::read(fixture.directory.join("service.exe")).unwrap(),
                if rollback { original } else { replacement }
            );
            assert_eq!(
                ScmLifecycle.observe(&fixture.start(5)).unwrap().runtime,
                if scenario == TransactionScenario::StoppedUpgrade {
                    zup_plan::ServiceRuntimeState::Stopped
                } else {
                    zup_plan::ServiceRuntimeState::Running
                }
            );
        }
    }

    #[cfg(feature = "test-launcher")]
    #[test]
    #[ignore = "child process of the isolated native transaction test"]
    fn native_transaction_child() {
        native::transaction_child();
    }
}
