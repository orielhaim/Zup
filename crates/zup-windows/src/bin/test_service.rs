#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    let name = std::env::args().nth(1).expect("service name");
    fixture::dispatch(name)
}

#[cfg(not(windows))]
fn main() {
    panic!("the SCM fixture requires Windows");
}

#[cfg(windows)]
mod fixture {
    use std::{ffi::OsString, sync::mpsc, time::Duration};
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };

    define_windows_service!(service_main, run);

    pub fn dispatch(name: String) -> windows_service::Result<()> {
        windows_service::service_dispatcher::start(name, service_main)
    }

    fn run(_: Vec<OsString>) {
        execute().expect("SCM fixture status publication");
    }

    fn execute() -> windows_service::Result<()> {
        let name = std::env::args().nth(1).expect("service name");
        let mode = if std::fs::read(std::env::current_exe().expect("fixture executable"))
            .expect("fixture contents")
            .ends_with(b"zup-native-start-failure")
        {
            "fail".to_owned()
        } else {
            std::env::args().nth(2).unwrap_or_default()
        };
        let (stop, stopped) = mpsc::channel();
        let handle = service_control_handler::register(name, move |control| match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => {
                let _ = stop.send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        let publish = |state, checkpoint, code| {
            handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: if state == ServiceState::Running {
                    ServiceControlAccept::STOP
                } else {
                    ServiceControlAccept::empty()
                },
                exit_code: ServiceExitCode::Win32(code),
                checkpoint,
                wait_hint: Duration::from_secs(2),
                process_id: None,
            })
        };
        for checkpoint in 1..=if mode == "slow" { 20 } else { 3 } {
            publish(ServiceState::StartPending, checkpoint, 0)?;
            std::thread::park_timeout(Duration::from_millis(100));
        }
        if mode == "fail" {
            return publish(ServiceState::Stopped, 0, 1066);
        }
        publish(ServiceState::Running, 0, 0)?;
        if mode == "crash" {
            if stopped.recv_timeout(Duration::from_millis(500)).is_err() {
                std::process::exit(42);
            }
        } else {
            let _ = stopped.recv();
        }
        for checkpoint in 1..=3 {
            publish(ServiceState::StopPending, checkpoint, 0)?;
            std::thread::park_timeout(Duration::from_millis(100));
        }
        publish(ServiceState::Stopped, 0, 0)
    }
}
