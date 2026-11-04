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
    ) -> Result<(), String> {
        operation.validate()?;
        let current = self.observe(operation)?;
        if current.configuration != operation.configuration
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
}
