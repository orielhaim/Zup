use std::time::{Duration, Instant};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
#[cfg(all(test, feature = "test-launcher"))]
use zup_plan::{NodeKind, ServiceControlStage};
use zup_plan::{ServiceControlOperation, ServiceTerminalState};
use zup_transaction::{NativeServiceManager, ServiceObservation};

#[derive(Default)]
pub(crate) struct ScmLifecycle {
    scopes: std::collections::BTreeMap<String, crate::service_scope::ServiceScope>,
    verified_empty: std::collections::BTreeSet<String>,
    registrations: std::collections::BTreeMap<
        String,
        (
            zup_core::ServiceOwnershipClaim,
            windows_service::service::Service,
        ),
    >,
}

impl ScmLifecycle {
    pub(crate) fn retain(
        &mut self,
        name: &str,
        claim: &zup_core::ServiceOwnershipClaim,
    ) -> Result<(), String> {
        let key = name.to_ascii_lowercase();
        if let Some((retained, service)) = self.registrations.get(&key) {
            if retained != claim
                || crate::service_claim::observe(name, service)
                    .map_err(|error| error.to_string())?
                    != (zup_core::ObservedServiceRegistration::Scm {
                        claim: claim.clone(),
                    })
            {
                return Err("retained service ownership changed".into());
            }
            return Ok(());
        }
        let service = open(name, ServiceAccess::QUERY_STATUS)?;
        if crate::service_claim::observe(name, &service).map_err(|error| error.to_string())?
            != (zup_core::ObservedServiceRegistration::Scm {
                claim: claim.clone(),
            })
        {
            return Err("service ownership differs from the authorized installation".into());
        }
        self.registrations.insert(key, (claim.clone(), service));
        Ok(())
    }

    pub(crate) fn release_for_removal(
        &mut self,
        name: &str,
        deletion_handle: &windows_service::service::Service,
        claim: &zup_core::ServiceOwnershipClaim,
    ) -> Result<(), String> {
        if crate::service_claim::observe(name, deletion_handle)
            .map_err(|error| error.to_string())?
            != (zup_core::ObservedServiceRegistration::Scm {
                claim: claim.clone(),
            })
        {
            return Err("service ownership changed before removal".into());
        }
        let key = name.to_ascii_lowercase();
        if self
            .registrations
            .get(&key)
            .is_some_and(|(retained, _)| retained != claim)
        {
            return Err("service removal conflicts with the retained installation claim".into());
        }
        self.registrations.remove(&key);
        Ok(())
    }
}

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

fn ownership_claim(
    operation: &ServiceControlOperation,
) -> Result<zup_core::ServiceOwnershipClaim, String> {
    let zup_core::ResourceKey::Service { id } = &operation.key else {
        return Err("logical service resource required".into());
    };
    operation
        .registration
        .claim(id.clone())
        .ok_or_else(|| "SCM control requires an installation ownership claim".into())
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

pub(crate) fn process_generation(
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
    fn prepare_recovery(
        &mut self,
        operation: &ServiceControlOperation,
        deadline: Instant,
    ) -> Result<(), String> {
        if operation.intent != zup_plan::ServiceControlIntent::Transition
            || operation.stage != zup_plan::ServiceControlStage::Restore
            || operation.before_configuration == operation.configuration
        {
            return Ok(());
        }
        if zup_plan::disabled_startup(&operation.configuration).as_ref()
            != Some(&operation.before_configuration)
        {
            return Err("SCM startup recovery has no authorized closed gate".into());
        }
        self.restore_process_scope(operation, &operation.process_scope)?;
        let current = self.observe(operation)?;
        if current.runtime != zup_plan::ServiceRuntimeState::Stopped {
            return Ok(());
        }
        if current.configuration == operation.before_configuration {
            if !self
                .scopes
                .contains_key(&operation.native_name.to_ascii_lowercase())
                && !self
                    .verified_empty
                    .contains(&operation.native_name.to_ascii_lowercase())
            {
                return Err("service startup outcome has no retained process scope; restart Windows, then recover this transaction before replacing files".into());
            }
            return Ok(());
        }
        if current.configuration != operation.configuration || Instant::now() >= deadline {
            return Err(
                "SCM startup registration changed before recovery admission was closed".into(),
            );
        }
        crate::services::change_live_configuration(
            &operation.native_name,
            &operation.configuration,
            &operation.before_configuration,
            &ownership_claim(operation)?,
        )
        .map_err(|error| error.to_string())?;
        let closed = self.observe(operation)?;
        if closed.configuration != operation.before_configuration
            || closed.runtime != zup_plan::ServiceRuntimeState::Stopped
            || closed.manager_generation != current.manager_generation
        {
            return Err("SCM startup gate did not remain closed during recovery".into());
        }
        if !self
            .scopes
            .contains_key(&operation.native_name.to_ascii_lowercase())
            && !self
                .verified_empty
                .contains(&operation.native_name.to_ascii_lowercase())
        {
            return Err("service startup admission is closed, but its process scope was lost; restart Windows, then recover this transaction before replacing files".into());
        }
        Ok(())
    }

    fn restore_process_scope(
        &mut self,
        operation: &ServiceControlOperation,
        scope: &zup_plan::ServiceScopeObservation,
    ) -> Result<(), String> {
        let current_manager = manager_generation()?;
        if current_manager != operation.manager_generation {
            let zup_plan::ServiceScopeObservation::WindowsJob { boot_sequence, .. } = scope else {
                return Err("SCM recovery lacks native boot sequence evidence".into());
            };
            if crate::service_scope::boot_sequence()? <= *boot_sequence {
                return Err("SCM changed without a verified later native boot sequence".into());
            }
            self.verified_empty
                .insert(operation.native_name.to_ascii_lowercase());
        }
        if matches!(
            scope,
            zup_plan::ServiceScopeObservation::WindowsJob {
                state: zup_plan::WindowsJobState::Empty,
                ..
            }
        ) {
            self.verified_empty
                .insert(operation.native_name.to_ascii_lowercase());
        }
        Ok(())
    }

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
        let deadline = Instant::now() + operation.timeout_for(ServiceTerminalState::Stopped);
        loop {
            if self.stopped_runtime_is_quiescent(operation, &operation.configuration)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "SCM stopped but the native service process scope did not drain".into(),
                );
            }
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        if !self.stopped_runtime_is_quiescent(operation, &operation.configuration)? {
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
            && service_processes_exited(operation)?
            && {
                let key = operation.native_name.to_ascii_lowercase();
                match &current.process_scope {
                    zup_plan::ServiceScopeObservation::WindowsJob {
                        state: zup_plan::WindowsJobState::Empty,
                        ..
                    } => {
                        self.scopes.get(&key).is_some_and(|scope| {
                            scope.was_member(&operation.before_execution_generation)
                            || matches!(&operation.process_scope, zup_plan::ServiceScopeObservation::WindowsJob {
                                state: zup_plan::WindowsJobState::Member { execution_generation }, ..
                            } if scope.was_member(execution_generation))
                        }) || self.verified_empty.contains(&key)
                    }
                    zup_plan::ServiceScopeObservation::WindowsJob {
                        state: zup_plan::WindowsJobState::Absent, ..
                    } => {
                        self.verified_empty.contains(&key)
                            || operation.before_execution_generation == "stopped"
                                && operation.stage == zup_plan::ServiceControlStage::Quiesce
                                && matches!(
                                    operation.process_scope,
                                    zup_plan::ServiceScopeObservation::WindowsJob {
                                        state: zup_plan::WindowsJobState::Absent,
                                        ..
                                    }
                                )
                    }
                    _ => false,
                }
            })
    }
    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String> {
        self.retain(&operation.native_name, &ownership_claim(operation)?)?;
        let target = match &operation.configuration {
            zup_plan::ServiceState::Registration { command, .. } => command.executable.target(),
            _ => return Err("service control requires registration".into()),
        };
        let service = open(&operation.native_name, ServiceAccess::QUERY_STATUS)?;
        let registration = crate::scm::query_service(&operation.native_name, target)
            .map_err(|e| format!("{e}: {e:?}"))?;
        if registration.registration
            != (zup_core::ObservedServiceRegistration::Scm {
                claim: ownership_claim(operation)?,
            })
        {
            return Err("native service ownership differs from the authorized installation".into());
        }
        let configuration = registration
            .state
            .configuration()
            .ok_or("unrecognized SCM registration")?;
        let deadline = Instant::now() + operation.timeout_for(operation.after);
        loop {
            let status = service.query_status().map_err(|error| error.to_string())?;
            match status_generation_snapshot(&status) {
                Ok(execution_generation) => {
                    let checked = service.query_status().map_err(|error| error.to_string())?;
                    if checked.current_state == status.current_state
                        && checked.process_id == status.process_id
                    {
                        let key = operation.native_name.to_ascii_lowercase();
                        let process_scope = if status.current_state == ServiceState::Running {
                            let zup_plan::ServiceState::Registration { command, .. } =
                                &configuration
                            else {
                                return Err("service image registration is missing".into());
                            };
                            match self.scopes.entry(key.clone()) {
                                std::collections::btree_map::Entry::Occupied(mut entry) => {
                                    entry.get_mut().bind_member(
                                        operation,
                                        status.process_id.ok_or("service PID unavailable")?,
                                        &execution_generation,
                                        &command.executable,
                                    )?;
                                }
                                std::collections::btree_map::Entry::Vacant(entry) => {
                                    let mut scope =
                                        crate::service_scope::ServiceScope::open(operation)?
                                            .ok_or("running service has no declared native job")?;
                                    scope.bind_member(
                                        operation,
                                        status.process_id.ok_or("service PID unavailable")?,
                                        &execution_generation,
                                        &command.executable,
                                    )?;
                                    entry.insert(scope);
                                }
                            }
                            crate::service_scope::observation(zup_plan::WindowsJobState::Member {
                                execution_generation: execution_generation.clone(),
                            })?
                        } else if let Some(scope) = self.scopes.get(&key) {
                            crate::service_scope::observation(scope.observe(operation)?)?
                        } else {
                            match crate::service_scope::ServiceScope::open(operation)? {
                                None => crate::service_scope::observation(
                                    if self.verified_empty.contains(&key) {
                                        zup_plan::WindowsJobState::Empty
                                    } else {
                                        zup_plan::WindowsJobState::Absent
                                    },
                                )?,
                                Some(mut scope) => {
                                    let member = match &operation.process_scope {
                                        zup_plan::ServiceScopeObservation::WindowsJob {
                                            state:
                                                zup_plan::WindowsJobState::Member {
                                                    execution_generation,
                                                },
                                            ..
                                        } => Some(execution_generation.as_str()),
                                        _ => None,
                                    };
                                    if let Some(member) = member
                                        && let Some(pid) = member
                                            .split(':')
                                            .next()
                                            .and_then(|pid| pid.parse::<u32>().ok())
                                        && crate::service_control::execution_generation(pid)
                                            .ok()
                                            .as_deref()
                                            == Some(member)
                                    {
                                        let zup_plan::ServiceState::Registration {
                                            command, ..
                                        } = &operation.before_configuration
                                        else {
                                            return Err(
                                                "source service image registration is unavailable"
                                                    .into(),
                                            );
                                        };
                                        scope.bind_member(
                                            operation,
                                            pid,
                                            member,
                                            &command.executable,
                                        )?;
                                        let observation = crate::service_scope::observation(
                                            scope.observe(operation)?,
                                        )?;
                                        self.scopes.insert(key.clone(), scope);
                                        observation
                                    } else {
                                        crate::service_scope::observation(
                                            scope.observe(operation)?,
                                        )?
                                    }
                                }
                            }
                        };
                        return Ok(ServiceObservation {
                            registration: registration.registration,
                            configuration,
                            runtime: runtime(status.current_state),
                            manager_generation: manager_generation()?,
                            execution_generation,
                            process_scope,
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
                &ownership_claim(operation)?,
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
        if crate::service_claim::observe(&operation.native_name, &service)
            .map_err(|error| error.to_string())?
            != (zup_core::ObservedServiceRegistration::Scm {
                claim: ownership_claim(operation)?,
            })
        {
            return Err("service ownership changed immediately before native control".into());
        }
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
                let key = operation.native_name.to_ascii_lowercase();
                if let Some(scope) = self.scopes.get(&key)
                    && scope.observe(operation)? != zup_plan::WindowsJobState::Empty
                {
                    return Err(
                        "previous service job still contains processes; activation is deferred"
                            .into(),
                    );
                }
                self.scopes.remove(&key);
                self.verified_empty.remove(&key);
                if crate::service_scope::ServiceScope::open(operation)?.is_some() {
                    return Err("previous service job still has live handles; cannot activate a new generation".into());
                }
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
        use zup_transaction::{apply_service_control, verify_service_control};

        struct Fixture {
            registration: zup_core::NativeServiceRegistration,
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
                let installation = zup_core::InstallationId::new_v7();
                let job = zup_core::service_job_name(
                    installation,
                    &zup_core::ServiceId::new(&name).unwrap(),
                );
                let configuration = zup_plan::ServiceState::Registration {
                    state_directories: Vec::new(),
                    native_timeouts: zup_core::NativeServiceTimeouts::Scm,
                    dependencies,
                    failure,
                    execution: zup_core::ServiceExecution::WindowsJob { account },
                    readiness: zup_core::ServiceReadiness::Manager,
                    display_name: name.clone(),
                    command: Box::new(zup_core::CommandSpec::new(
                        zup_core::TargetPath::new(target, crate::plain_path_text(&executable))
                            .unwrap(),
                        vec![name.clone(), mode.into(), "--job".into(), job],
                    )),
                    start: zup_core::ServiceStart::Manual,
                };
                let mut fixture = Self {
                    registration: zup_core::NativeServiceRegistration::Scm { installation },
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
                crate::services::apply_native(
                    &zup_plan::NativeServiceOperation::Configure {
                        service: Box::new(zup_plan::ServiceOperation {
                            state_directories: Vec::new(),
                            registration: fixture.registration,
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
                            lifecycle: zup_core::ServiceLifecycle {
                                ..Default::default()
                            },
                            requires_files: vec![],
                            requires_services: Vec::new(),
                            dependency_names: dependencies,
                            privilege: zup_core::Privilege::System,
                            previous: zup_plan::ObservedServiceState::Absent,
                            conflict: None,
                        }),
                        previous: None,
                        activation: zup_plan::ServiceActivation::Declared,
                    },
                    &mut ScmLifecycle::default(),
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
                    requires_files: Vec::new(),
                    registration: self.registration,
                    intent: zup_plan::ServiceControlIntent::Transition,
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
                    process_scope: zup_plan::ServiceScopeObservation::WindowsJob {
                        boot_sequence: crate::service_scope::boot_sequence().unwrap(),
                        state: zup_plan::WindowsJobState::Absent,
                    },
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
                        .and_then(|observed| observed.state.configuration());
                    if current.as_ref() != Some(&self.configuration)
                        && current != zup_plan::disabled_startup(&self.configuration)
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
            let receipt = apply_service_control(&mut ScmLifecycle::default(), &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("service receipt");
            };
            verify_service_control(&mut ScmLifecycle::default(), &receipt).unwrap();
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
            stop.process_scope = receipt.process_scope.clone();
            let stopped = apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(stopped) = stopped else {
                panic!("service receipt");
            };
            verify_service_control(&mut ScmLifecycle::default(), &stopped).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(fixture.directory.join("service.exe"))
                .unwrap();
            apply_service_control(&mut ScmLifecycle::default(), &fixture.start(5)).unwrap();
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
            let receipt = apply_service_control(&mut ScmLifecycle::default(), &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("native service receipt")
            };
            let mut generations =
                std::collections::BTreeSet::from([receipt.execution_generation.clone()]);
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stopped_since = None;
            loop {
                let observation = ScmLifecycle::default().observe(&start).unwrap();
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
        fn stopped_main_does_not_authorize_mutation_with_remaining_job_workers() {
            let fixture = Fixture::new(
                "job-orphan",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let mut manager = ScmLifecycle::default();
            let start = fixture.start(5);
            let zup_transaction::OperationReceipt::ServiceControl(started) =
                apply_service_control(&mut manager, &start).unwrap()
            else {
                panic!("native service receipt")
            };
            let mut stop = start;
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = started.execution_generation;
            stop.process_scope = started.process_scope;
            stop.configuration = zup_plan::disabled_startup(&stop.configuration).unwrap();
            stop.timeouts = zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 5,
                stop_observation_seconds: 1,
            };
            assert!(apply_service_control(&mut manager, &stop).is_err());
            let observed = manager.observe(&stop).unwrap();
            assert_eq!(observed.runtime, zup_plan::ServiceRuntimeState::Stopped);
            assert!(matches!(
                observed.process_scope,
                zup_plan::ServiceScopeObservation::WindowsJob {
                    state: zup_plan::WindowsJobState::Occupied { .. },
                    ..
                }
            ));
            assert!(
                !manager
                    .stopped_runtime_is_quiescent(&stop, &stop.configuration)
                    .unwrap()
            );
            drop(manager);
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
            let receipt = apply_service_control(&mut ScmLifecycle::default(), &start).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                panic!("native service receipt")
            };
            apply_service_control(&mut ScmLifecycle::default(), &dependent.start(5)).unwrap();
            let mut stop = start;
            stop.stage = ServiceControlStage::Quiesce;
            stop.before = ServiceTerminalState::Running;
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = receipt.execution_generation.clone();
            stop.process_scope = receipt.process_scope.clone();
            assert!(apply_service_control(&mut ScmLifecycle::default(), &stop).is_err());
            let current = ScmLifecycle::default().observe(&stop).unwrap();
            assert_eq!(current.runtime, zup_plan::ServiceRuntimeState::Running);
            assert_eq!(current.execution_generation, receipt.execution_generation);
            drop(dependent);
            apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap();
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
                assert!(apply_service_control(&mut ScmLifecycle::default(), &start).is_err());
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
                apply_service_control(&mut ScmLifecycle::default(), &start).unwrap();
            }
            if queued {
                let deadline = Instant::now() + Duration::from_secs(3);
                while ScmLifecycle::default().observe(&start).unwrap().runtime
                    != zup_plan::ServiceRuntimeState::Stopped
                {
                    assert!(Instant::now() < deadline);
                    std::thread::park_timeout(Duration::from_millis(20));
                }
            }
            let before = ScmLifecycle::default().observe(&start).unwrap();
            let mut stop = start.clone();
            stop.stage = ServiceControlStage::Quiesce;
            stop.configuration = zup_plan::disabled_startup(&stop.before_configuration).unwrap();
            stop.before = if initially_stopped {
                ServiceTerminalState::Stopped
            } else {
                ServiceTerminalState::Running
            };
            stop.after = ServiceTerminalState::Stopped;
            stop.before_execution_generation = before.execution_generation;
            stop.process_scope = before.process_scope.clone();
            let zup_transaction::OperationReceipt::ServiceControl(stopped) =
                apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap()
            else {
                panic!("shutdown receipt")
            };
            let mut recovered = ScmLifecycle::default();
            verify_service_control(&mut recovered, &stopped).unwrap();
            assert!(
                recovered
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
                    apply_service_control(&mut ScmLifecycle::default(), &release)
                        .unwrap_err()
                        .contains("pending recovery actions")
                );
                assert!(
                    recovered
                        .stopped_runtime_is_quiescent(&stop, &stop.configuration)
                        .unwrap()
                );
                return;
            }
            let zup_transaction::OperationReceipt::ServiceControl(released) =
                apply_service_control(&mut ScmLifecycle::default(), &release).unwrap()
            else {
                panic!("restoration receipt")
            };
            verify_service_control(&mut ScmLifecycle::default(), &released).unwrap();
            zup_transaction::rollback_service_control(&mut ScmLifecycle::default(), &released)
                .unwrap();
            zup_transaction::rollback_service_control(&mut ScmLifecycle::default(), &stopped)
                .unwrap();
            let restored = ScmLifecycle::default().observe(&start).unwrap();
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
                apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap()
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
            stop.process_scope = started.process_scope.clone();
            let receipt = apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap();
            let zup_transaction::OperationReceipt::ServiceControl(stopped) = receipt else {
                unreachable!()
            };
            verify_service_control(&mut ScmLifecycle::default(), &stopped).unwrap();
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
                apply_service_control(&mut ScmLifecycle::default(), &operation).unwrap()
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
            operation.process_scope = started.process_scope.clone();
            operation.configuration = zup_plan::disabled_startup(&fixture.configuration).unwrap();
            crate::services::change_live_configuration(
                &fixture.name,
                &fixture.configuration,
                &operation.configuration,
                &ownership_claim(&operation).unwrap(),
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
            let mut recovered = ScmLifecycle::default();
            assert!(
                !recovered
                    .stopped_runtime_is_quiescent(&operation, &operation.configuration)
                    .unwrap()
            );
            assert!(matches!(
                zup_transaction::reconcile_service_control(&mut recovered, &operation, None),
                Err(_) | Ok(zup_transaction::ReconcileResult::Ambiguous)
            ));
            drop(release);
            // SAFETY: Wait only observes the retained process; it cannot terminate it.
            assert_eq!(
                unsafe { WaitForSingleObject(*process, 10000) },
                WAIT_OBJECT_0
            );
            assert!(matches!(
                zup_transaction::reconcile_service_control(&mut recovered, &operation, None)
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
            assert!(apply_service_control(&mut ScmLifecycle::default(), &failed.start(5)).is_err());
            let slow = Fixture::new(
                "slow",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            assert!(apply_service_control(&mut ScmLifecycle::default(), &slow.start(1)).is_err());
            let slow_stop = Fixture::new(
                "slow-stop",
                zup_core::WindowsServiceAccount::LocalService,
                Default::default(),
                Vec::new(),
            );
            let mut stop = slow_stop.start(5);
            let receipt = apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap();
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
            stop.process_scope = receipt.process_scope.clone();
            stop.timeouts = zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 5,
                stop_observation_seconds: 1,
            };
            assert!(apply_service_control(&mut ScmLifecycle::default(), &stop).is_err());
            let pending = ScmLifecycle::default().observe(&stop).unwrap();
            assert_eq!(pending.runtime, zup_plan::ServiceRuntimeState::StopPending);
            // SAFETY: The retained process handle has synchronization access.
            assert_eq!(unsafe { WaitForSingleObject(*process, 0) }, WAIT_TIMEOUT);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if ScmLifecycle::default().observe(&stop).unwrap().runtime
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
            lifecycle: ScmLifecycle,
            files: crate::WindowsFileExecutor<zup_bundle::DirectoryPayloadSource>,
            fail_readiness: bool,
            abort_after_replacement: bool,
        }

        impl zup_transaction::OperationExecutor for NativeTransaction {
            type Error = String;
            fn prepare(&mut self, _: &zup_plan::DeploymentNode) -> Result<(), String> {
                Ok(())
            }
            fn apply(
                &mut self,
                node: &zup_plan::DeploymentNode,
                transaction: &zup_transaction::TransactionRecord,
            ) -> Result<zup_transaction::OperationReceipt, String> {
                use zup_transaction::OperationReceipt;
                match &node.kind {
                    NodeKind::ServiceControl { operation } => {
                        apply_service_control(&mut self.lifecycle, operation)
                    }
                    NodeKind::Barrier => Ok(OperationReceipt::Control),
                    NodeKind::StageFile { .. } | NodeKind::FileMutation { .. } => {
                        let receipt = crate::apply_node(&mut self.files, node, transaction)
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
                node: &zup_plan::DeploymentNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<(), String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        verify_service_control(&mut self.lifecycle, receipt)?;
                        if self.fail_readiness
                            && receipt.operation.stage == ServiceControlStage::Restore
                        {
                            self.fail_readiness = false;
                            return Err("injected failure after native readiness".into());
                        }
                        Ok(())
                    }
                    zup_transaction::OperationReceipt::Control => Ok(()),
                    _ => crate::verify_installed_file(node, receipt)
                        .map_err(|error| error.to_string()),
                }
            }
            fn rollback(
                &mut self,
                node: &zup_plan::DeploymentNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<(), String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        zup_transaction::rollback_service_control(&mut self.lifecycle, receipt)
                    }
                    zup_transaction::OperationReceipt::Control => Ok(()),
                    _ => self
                        .files
                        .rollback_transaction_receipt(node, receipt)
                        .map_err(|error| error.to_string()),
                }
            }
            fn prepare_compensation(
                &mut self,
                _: &zup_plan::DeploymentNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<zup_transaction::OperationReceipt, String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        zup_transaction::prepare_service_compensation(&mut self.lifecycle, receipt)
                            .map(|receipt| {
                                zup_transaction::OperationReceipt::ServiceControl(Box::new(receipt))
                            })
                    }
                    _ => Ok(receipt.clone()),
                }
            }
            fn reconcile(
                &mut self,
                node: &zup_plan::DeploymentNode,
                receipt: Option<&zup_transaction::OperationReceipt>,
                transaction: &zup_transaction::TransactionRecord,
            ) -> Result<zup_transaction::ReconcileResult, String> {
                if let zup_plan::NodeKind::ServiceControl { operation } = &node.kind {
                    let receipt = match receipt {
                        Some(zup_transaction::OperationReceipt::ServiceControl(receipt)) => {
                            Some(receipt.as_ref())
                        }
                        _ => None,
                    };
                    return zup_transaction::reconcile_service_control(
                        &mut self.lifecycle,
                        operation,
                        receipt,
                    );
                }
                self.files
                    .reconcile_transaction_node(node, receipt, transaction)
                    .map_err(|error| error.to_string())
            }
            fn reconcile_rollback(
                &mut self,
                _: &zup_plan::DeploymentNode,
                receipt: &zup_transaction::OperationReceipt,
            ) -> Result<zup_transaction::RollbackReconcileResult, String> {
                match receipt {
                    zup_transaction::OperationReceipt::ServiceControl(receipt) => {
                        zup_transaction::reconcile_service_rollback(&mut self.lifecycle, receipt)
                    }
                    _ => Ok(zup_transaction::RollbackReconcileResult::Retry),
                }
            }
        }

        #[derive(Default)]
        enum PublicationFault {
            #[default]
            None,
            Before,
            After,
        }
        #[derive(Default)]
        struct MemoryInstallation(
            std::sync::Mutex<Option<zup_plan::InstallationRecord>>,
            std::sync::Mutex<PublicationFault>,
        );

        impl zup_transaction::InstallationPublisher for MemoryInstallation {
            type Error = String;
            fn read_current(
                &self,
                _: &zup_transaction::TransactionRecord,
            ) -> Result<Option<zup_plan::InstallationRecord>, String> {
                Ok(self.0.lock().unwrap().clone())
            }

            fn prepare(
                &self,
                transaction: &zup_transaction::TransactionRecord,
            ) -> Result<zup_plan::InstallationRecord, String> {
                zup_transaction::prepare_installation_record(
                    transaction,
                    self.0.lock().unwrap().as_ref(),
                )
                .map_err(|error| error.to_string())
            }
            fn publish(
                &self,
                transaction: &zup_transaction::TransactionRecord,
                observed: &zup_transaction::PublicationObservation,
            ) -> Result<zup_plan::InstallationRecord, String> {
                if matches!(*self.1.lock().unwrap(), PublicationFault::Before) {
                    return Err("publication deferred".into());
                }
                let mut durable = self.0.lock().unwrap();
                observed.check_current(transaction, durable.clone())?;
                let installation =
                    zup_transaction::prepare_installation_record(transaction, durable.as_ref())
                        .map_err(|error| error.to_string())?;
                *durable = Some(installation.clone());
                if matches!(*self.1.lock().unwrap(), PublicationFault::After) {
                    return Err("publication acknowledgment lost".into());
                }
                Ok(installation)
            }
        }

        fn transaction_executor(
            record: &zup_transaction::TransactionRecord,
            root: &std::path::Path,
        ) -> NativeTransaction {
            let files = crate::WindowsFileExecutor::new(
                zup_bundle::DirectoryPayloadSource::new(root.join("payload")),
                record.transaction_id,
                ".exe",
            );
            NativeTransaction {
                lifecycle: ScmLifecycle::default(),
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
                .execute(record, &mut executor, &MemoryInstallation::default())
                .unwrap();
            panic!("the replacement crash did not fire");
        }

        #[derive(serde::Serialize, serde::Deserialize)]
        struct NativeRebootFixture {
            root: PathBuf,
            transaction: zup_core::TransactionId,
            control: ServiceControlOperation,
            original_sha256: String,
        }

        pub(super) fn recover_after_boot() {
            use zup_transaction::TransactionStore;
            let state: NativeRebootFixture = serde_json::from_slice(
                &std::fs::read(std::env::var_os("ZUP_SCM_REBOOT_STATE").unwrap()).unwrap(),
            )
            .unwrap();
            let store = zup_transaction::FilesystemTransactionStore::private_at(
                state.root.join("journals"),
            );
            let record = store.load(&state.transaction).unwrap();
            let mut executor = transaction_executor(&record, &state.root);
            let (_, outcome) = zup_transaction::recover(
                record,
                &store,
                &mut executor,
                &MemoryInstallation::default(),
            )
            .unwrap();
            assert_eq!(outcome, zup_transaction::TransactionOutcome::RolledBack);
            let current = ScmLifecycle::default().observe(&state.control).unwrap();
            assert_eq!(current.runtime, zup_plan::ServiceRuntimeState::Running);
            assert_eq!(current.configuration, state.control.before_configuration);
            let zup_plan::ServiceState::Registration { command, .. } = &current.configuration
            else {
                unreachable!()
            };
            let executable = crate::lowering::host_path(&command.executable);
            assert_eq!(
                zup_core::hash_bytes(&std::fs::read(&executable).unwrap()).to_hex(),
                state.original_sha256
            );
            let fixture = Fixture {
                name: state.control.native_name.clone(),
                directory: executable.parent().unwrap().to_owned(),
                registration: state.control.registration,
                configuration: current.configuration,
                service: Some(
                    open(
                        &state.control.native_name,
                        ServiceAccess::QUERY_STATUS
                            | ServiceAccess::QUERY_CONFIG
                            | ServiceAccess::STOP
                            | ServiceAccess::DELETE,
                    )
                    .unwrap(),
                ),
            };
            drop(executor);
            drop(fixture);
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum TransactionScenario {
            RunningUpgrade,
            StoppedUpgrade,
            ReadinessFailure,
            FailedStartUpgrade,
            CrashAfterReplacement,
            PublicationDeferred,
            PublicationAcknowledgmentLost,
        }

        #[rstest::rstest]
        #[case(TransactionScenario::RunningUpgrade)]
        #[case(TransactionScenario::StoppedUpgrade)]
        #[case(TransactionScenario::ReadinessFailure)]
        #[case(TransactionScenario::FailedStartUpgrade)]
        #[case(TransactionScenario::CrashAfterReplacement)]
        #[case(TransactionScenario::PublicationDeferred)]
        #[case(TransactionScenario::PublicationAcknowledgmentLost)]
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
            let target = zup_core::Target::host().unwrap();
            let zup_core::NativeServiceRegistration::Scm { installation } = fixture.registration
            else {
                unreachable!()
            };
            let mut input = zup_plan::DeploymentEffects::new(
                installation,
                crate::requirements_fixture::requirements(
                    target,
                    zup_core::SelectedScope::Machine,
                    "com.example.scm-transaction",
                    "1.0.0",
                ),
            );
            input.install_directory = Some(
                zup_core::TargetPath::new(target, crate::plain_path_text(&fixture.directory))
                    .unwrap(),
            );
            if scenario != TransactionScenario::StoppedUpgrade {
                let start = fixture.start(5);
                let receipt = apply_service_control(&mut ScmLifecycle::default(), &start).unwrap();
                let zup_transaction::OperationReceipt::ServiceControl(receipt) = receipt else {
                    unreachable!()
                };
                let mut stop = start.clone();
                stop.stage = ServiceControlStage::Quiesce;
                stop.before = ServiceTerminalState::Running;
                stop.after = ServiceTerminalState::Stopped;
                stop.before_execution_generation = receipt.execution_generation;
                stop.process_scope = receipt.process_scope.clone();
                stop.configuration = zup_plan::disabled_startup(&stop.configuration).unwrap();
                let mut restore = start;
                restore.before_configuration = stop.configuration.clone();
                input.service_controls.extend([stop, restore]);
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
                replacement: zup_core::FixedReplacement::Quiescent,
                key: zup_core::ResourceKey::File {
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::File {
                            declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                        },
                    },
                },
                destination: command.executable.clone(),
                source_relative: zup_core::RelativePath::new("service.exe").unwrap(),
                precondition: zup_plan::FilePrecondition::Exact {
                    object: match crate::inspect_file(&fixture.directory.join("service.exe"))
                        .unwrap()
                    {
                        zup_plan::ObservedFileState::File { object, .. } => object,
                        _ => panic!("the service fixture must be a regular file"),
                    },
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
                zup_core::TransactionId::new_v7(),
                zup_core::AppId::new("com.example.scm-transaction").unwrap(),
                zup_core::SelectedScope::Machine,
                "1.0.0".parse().unwrap(),
                zup_plan::compile_deployment(&input).unwrap(),
            );
            let store = zup_transaction::FilesystemTransactionStore::private_at(
                root.path().join("journals"),
            );
            store.create(&record).unwrap();
            let mut executor = transaction_executor(&record, root.path());
            executor.fail_readiness = scenario == TransactionScenario::ReadinessFailure;
            if matches!(
                scenario,
                TransactionScenario::PublicationDeferred
                    | TransactionScenario::PublicationAcknowledgmentLost
            ) {
                let published = scenario == TransactionScenario::PublicationAcknowledgmentLost;
                let publisher = MemoryInstallation(
                    Default::default(),
                    std::sync::Mutex::new(if published {
                        PublicationFault::After
                    } else {
                        PublicationFault::Before
                    }),
                );
                let (pending, outcome) =
                    zup_transaction::TransactionCoordinator::new(store.clone())
                        .execute(record, &mut executor, &publisher)
                        .unwrap();
                assert_eq!(
                    outcome,
                    zup_transaction::TransactionOutcome::RecoveryRequired
                );
                drop(executor);
                let mut stop = fixture.start(5);
                let current = ScmLifecycle::default().observe(&stop).unwrap();
                stop.stage = ServiceControlStage::Quiesce;
                stop.before = ServiceTerminalState::Running;
                stop.after = ServiceTerminalState::Stopped;
                stop.before_execution_generation = current.execution_generation;
                stop.process_scope = current.process_scope;
                stop.configuration = zup_plan::disabled_startup(&stop.configuration).unwrap();
                apply_service_control(&mut ScmLifecycle::default(), &stop).unwrap();
                *publisher.1.lock().unwrap() = PublicationFault::None;
                let mut executor = transaction_executor(&pending, root.path());
                let (recovered, outcome) =
                    zup_transaction::recover(pending, &store, &mut executor, &publisher).unwrap();
                assert_eq!(
                    outcome,
                    if published {
                        zup_transaction::TransactionOutcome::Committed
                    } else {
                        zup_transaction::TransactionOutcome::RecoveryRequired
                    },
                    "{:?}",
                    recovered.recovery_failure
                );
                assert_eq!(
                    ScmLifecycle::default().observe(&stop).unwrap().runtime,
                    zup_plan::ServiceRuntimeState::Stopped
                );
                assert_eq!(
                    std::fs::read(fixture.directory.join("service.exe")).unwrap(),
                    replacement
                );
                return;
            }
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
                    ScmLifecycle::default()
                        .observe(&fixture.start(5))
                        .unwrap()
                        .runtime,
                    zup_plan::ServiceRuntimeState::Stopped
                );
                zup_transaction::recover(
                    store.load(&record.transaction_id).unwrap(),
                    &store,
                    &mut executor,
                    &MemoryInstallation::default(),
                )
                .unwrap()
            } else {
                zup_transaction::TransactionCoordinator::new(store)
                    .execute(record, &mut executor, &MemoryInstallation::default())
                    .unwrap()
            };
            if scenario == TransactionScenario::FailedStartUpgrade {
                assert_eq!(
                    outcome,
                    zup_transaction::TransactionOutcome::RecoveryRequired
                );
                let current = ScmLifecycle::default().observe(&fixture.start(5)).unwrap();
                assert_eq!(current.runtime, zup_plan::ServiceRuntimeState::Stopped);
                assert_eq!(
                    current.configuration,
                    zup_plan::disabled_startup(&fixture.configuration).unwrap()
                );
                assert_eq!(
                    std::fs::read(fixture.directory.join("service.exe")).unwrap(),
                    replacement
                );
                if let Some(path) = std::env::var_os("ZUP_SCM_REBOOT_STATE") {
                    let control = input.service_controls[0].clone();
                    let state = NativeRebootFixture {
                        root: root.keep(),
                        transaction: settled.transaction_id,
                        control,
                        original_sha256: zup_core::hash_bytes(&original).to_hex(),
                    };
                    std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
                    std::mem::forget(fixture);
                }
                return;
            }
            let rollback = scenario == TransactionScenario::ReadinessFailure;
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
                ScmLifecycle::default()
                    .observe(&fixture.start(5))
                    .unwrap()
                    .runtime,
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
    #[ignore = "second phase of the isolated native reboot test"]
    fn native_reboot_recovery_child() {
        native::recover_after_boot();
    }

    #[cfg(feature = "test-launcher")]
    #[test]
    #[ignore = "child process of the isolated native transaction test"]
    fn native_transaction_child() {
        native::transaction_child();
    }
}
