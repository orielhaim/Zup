#[cfg(windows)]
#[path = "test_service/job.rs"]
mod service_job;

#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    let name = std::env::args().nth(1).expect("service name");
    let arguments: Vec<_> = std::env::args().collect();
    let _scope = arguments
        .iter()
        .position(|argument| argument == "--job")
        .map(|index| {
            let job = arguments.get(index + 1).expect("job name");
            let scope = service_job::create(&name, job).expect("native service job scope");
            assert!(service_job::mutation_denied(job).expect("restricted native scope"));
            scope
        });
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

    static FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub fn dispatch(name: String) -> windows_service::Result<()> {
        windows_service::service_dispatcher::start(name, service_main)?;
        while !FINISHED.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(20));
        }
        Ok(())
    }

    fn run(_: Vec<OsString>) {
        execute().expect("SCM fixture status publication");
        FINISHED.store(true, std::sync::atomic::Ordering::Release);
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
        if mode == "reported-never-started" {
            return publish(ServiceState::Stopped, 0, 1077);
        }
        if mode == "data" {
            let root =
                std::path::PathBuf::from(std::env::args_os().nth(3).expect("service data path"));
            let executable = std::env::current_exe().expect("native image path");
            std::fs::write(
                root.join("native-image.txt"),
                executable.to_string_lossy().as_bytes(),
            )
            .expect("service data access");
        }
        if mode == "job-foreign" {
            let arguments: Vec<_> = std::env::args().collect();
            let target = arguments
                .iter()
                .position(|argument| argument == "--probe-job")
                .and_then(|index| arguments.get(index + 1))
                .expect("foreign scope name");
            let denied =
                super::service_job::mutation_denied(target).expect("native scope access check");
            let root = std::path::PathBuf::from(std::env::args_os().nth(3).expect("data root"));
            std::fs::write(root.join("foreign-job-mutation-denied"), denied.to_string())
                .expect("scope probe output");
        }
        let mut child = if mode == "job-workers" || mode == "job-orphan" {
            use std::os::windows::process::CommandExt;
            Some(
                std::process::Command::new(
                    std::path::PathBuf::from(
                        std::env::var_os("SystemRoot").expect("Windows directory"),
                    )
                    .join("System32/PING.EXE"),
                )
                .args(["-n", "600", "127.0.0.1"])
                .creation_flags(0x08000000)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("contained native worker"),
            )
        } else {
            None
        };
        publish(ServiceState::Running, 0, 0)?;
        if mode == "crash" {
            if stopped.recv_timeout(Duration::from_millis(500)).is_err() {
                std::process::exit(42);
            }
        } else {
            let _ = stopped.recv();
        }
        if mode == "job-workers"
            && let Some(child) = &mut child
        {
            child.kill().expect("stop owned worker");
            child.wait().expect("join owned worker");
        }
        for checkpoint in 1..=if mode == "slow-stop" { 50 } else { 3 } {
            publish(ServiceState::StopPending, checkpoint, 0)?;
            std::thread::park_timeout(Duration::from_millis(100));
        }
        publish(ServiceState::Stopped, 0, 0)?;
        if mode == "linger-stop" {
            std::thread::park_timeout(Duration::from_secs(2));
        }
        if mode == "linger-stop-gated" {
            let release = std::env::current_exe()
                .expect("fixture executable")
                .with_extension("exit");
            while !release.exists() {
                std::thread::park_timeout(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}
