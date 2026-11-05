use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use private_fs::{FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile};
use serde::{Deserialize, Serialize};
use zup_bootstrap::BootstrapError;
use zup_core::PrerequisiteId;
use zup_runtime::{RuntimeControl, RuntimeEvent};

type Result<T> = std::result::Result<T, BootstrapError>;
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    program: String,
    state: ProcessState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ProcessState {
    Intended,
    NotStarted,
    Running {
        boot: String,
        group: u32,
        start_ticks: u64,
    },
    Exited {
        boot: String,
        group: u32,
        exit_code: Option<i32>,
    },
    Uncertain {
        boot: String,
        group: u32,
        start_ticks: u64,
        reason: String,
    },
}

pub(crate) struct PackageProcess<'a> {
    pub directory: &'a PrivateDir,
    pub id: &'a PrerequisiteId,
    pub program: &'a str,
    pub control: &'a RuntimeControl,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousProcess {
    Absent,
    NotStarted,
    Settled,
}

impl PackageProcess<'_> {
    fn stem(&self) -> String {
        format!(
            "native-{}",
            zup_core::hash_bytes(self.id.as_str().as_bytes())
        )
    }

    fn record(&self, state: ProcessState) -> Result<()> {
        let bytes = serde_json::to_vec(&Evidence {
            program: self.program.into(),
            state,
        })?;
        zup_transaction::publish_full(
            self.directory,
            format!("{}.json", self.stem()),
            &bytes,
            private_fs::Publication::Replace,
        )
        .map_err(|e| BootstrapError::Provider(e.to_string()))
    }

    pub fn check_previous(&self, interrupted: bool) -> Result<PreviousProcess> {
        let file = match self.directory.open_file(format!("{}.json", self.stem())) {
            Ok(file) => file,
            Err(private_fs::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return if interrupted {
                    Err(BootstrapError::RecoveryRequired(
                        "interrupted package launch has no durable process evidence".into(),
                    ))
                } else {
                    Ok(PreviousProcess::Absent)
                };
            }
            Err(error) => return Err(BootstrapError::RecoveryRequired(error.to_string())),
        };
        let mut bytes = Vec::new();
        file.take(8193).read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(BootstrapError::Limit(
                "native process evidence is too large",
            ));
        }
        let evidence: Evidence = serde_json::from_slice(&bytes)?;
        if evidence.program != self.program {
            return Err(BootstrapError::RecoveryRequired(
                "the native package provider changed".into(),
            ));
        }
        match evidence.state {
            ProcessState::Intended => Err(BootstrapError::RecoveryRequired("package launch was interrupted before its process identity was recorded; inspect the native package manager before recovery".into())),
            ProcessState::NotStarted => Ok(PreviousProcess::NotStarted),
            ProcessState::Running { boot, group, .. } | ProcessState::Exited { boot, group, .. } | ProcessState::Uncertain { boot, group, .. } => {
                uuid::Uuid::parse_str(&boot).map_err(|_| BootstrapError::RecoveryRequired("invalid native process boot identity".into()))?;
                let group = rustix::process::Pid::from_raw(group as i32)
                    .filter(|_| group <= i32::MAX as u32)
                    .ok_or_else(|| BootstrapError::RecoveryRequired("invalid native process group".into()))?;
                if boot != boot_id()? { return Ok(PreviousProcess::Settled); }
                match rustix::process::test_kill_process_group(group) {
                    Err(rustix::io::Errno::SRCH) => Ok(PreviousProcess::Settled),
                    Ok(()) | Err(rustix::io::Errno::PERM) => Err(BootstrapError::RecoveryRequired("the previous native package process group is still active; it has not been terminated".into())),
                    Err(error) => Err(BootstrapError::RecoveryRequired(format!("cannot observe the native package process group: {error}"))),
                }
            }
        }
    }

    pub fn execute(&self, command: &mut Command) -> Result<(ExitStatus, String)> {
        if self.control.cancellation.is_cancelled() {
            return Err(BootstrapError::ProviderPreflight(
                "package installation cancelled before launch".into(),
            ));
        }
        let previous = self.check_previous(false)?;
        let boot = boot_id()?;
        let log = self
            .directory
            .open_file_with(
                format!("{}.log", self.stem()),
                OpenOptions {
                    access: FileAccess::ReadWrite,
                    creation: if previous == PreviousProcess::Absent {
                        FileCreation::CreateNew
                    } else {
                        FileCreation::OpenOrCreate
                    },
                    truncate: true,
                    ..Default::default()
                },
            )
            .map_err(|e| BootstrapError::ProviderPreflight(e.to_string()))?;
        let flags = native_io(rustix::fs::fcntl_getfl(&log))?;
        native_io(rustix::fs::fcntl_setfl(
            &log,
            flags | rustix::fs::OFlags::APPEND,
        ))?;
        let output = || -> std::io::Result<std::fs::File> {
            Ok(rustix::io::fcntl_dupfd_cloexec(&log, 0)?.into())
        };
        command
            .stdin(Stdio::null())
            .stdout(output()?)
            .stderr(output()?)
            .process_group(0);
        self.record(ProcessState::Intended)?;
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.record(ProcessState::NotStarted)?;
                return Err(BootstrapError::ProviderPreflight(format!(
                    "native package manager did not launch: {error}"
                )));
            }
        };
        let mut child = Reaper(Some(child));
        let group = child.0.as_ref().unwrap().id();
        if let Some(status) = child.0.as_mut().unwrap().try_wait()? {
            child.0 = None;
            self.record(ProcessState::Exited {
                boot,
                group,
                exit_code: status.code(),
            })?;
            self.check_previous(false)?;
            return Ok((status, tail(&log)?));
        }
        let start_ticks = process_start(group)?;
        self.record(ProcessState::Running {
            boot: boot.clone(),
            group,
            start_ticks,
        })?;
        let started = Instant::now();
        let mut reported = None;
        loop {
            if let Some(status) = child.0.as_mut().unwrap().try_wait()? {
                child.0 = None;
                self.record(ProcessState::Exited {
                    boot,
                    group,
                    exit_code: status.code(),
                })?;
                self.check_previous(false)?;
                return Ok((status, tail(&log)?));
            }
            if native_io(rustix::fs::fstat(&log))?.st_size > MAX_LOG_BYTES as i64 {
                log.set_len(0)
                    .map_err(|e| BootstrapError::Provider(e.to_string()))?;
            }
            let elapsed = started.elapsed();
            if elapsed >= self.timeout {
                self.record(ProcessState::Uncertain {
                    boot,
                    group,
                    start_ticks,
                    reason: "installer deadline exceeded; native manager was not terminated".into(),
                })?;
                return Err(BootstrapError::RecoveryRequired(format!(
                    "native package installation exceeded {} seconds; process group {group} was left running to protect the package database. Recovery will re-observe it before proceeding. {}",
                    self.timeout.as_secs(),
                    tail(&log)?
                )));
            }
            let progress = (elapsed.as_secs(), self.control.cancellation.is_cancelled());
            if reported != Some(progress) {
                let _ = self.control.events.send(RuntimeEvent::Progress {
                    phase: zup_core::OperationPhase::System,
                    completed: 0,
                    total: 0,
                    action: if progress.1 {
                        "Cancellation deferred until the native package manager finishes safely"
                    } else {
                        "Waiting for the native package manager"
                    }
                    .into(),
                });
                reported = Some(progress);
            }
            std::thread::park_timeout(Duration::from_millis(100).min(self.timeout - elapsed));
        }
    }
}

struct Reaper(Option<Child>);
impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Package processes must survive installer deadlines without broken output pipes.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn boot_id() -> Result<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot = boot.trim();
    uuid::Uuid::parse_str(boot)
        .map_err(|_| BootstrapError::RecoveryRequired("invalid kernel boot identity".into()))?;
    Ok(boot.into())
}

fn process_start(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_ascii_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            BootstrapError::RecoveryRequired(
                "cannot identify the package process generation".into(),
            )
        })
}

fn tail(log: &PrivateFile) -> Result<String> {
    log.verify()
        .map_err(|e| BootstrapError::Provider(e.to_string()))?;
    let size = native_io(rustix::fs::fstat(log))?.st_size.max(0) as u64;
    let offset = size.saturating_sub(MAX_DIAGNOSTIC_BYTES as u64);
    let mut bytes = vec![0; (size - offset) as usize];
    let count = native_io(rustix::io::pread(log, &mut bytes, offset))?;
    bytes.truncate(count);
    log.verify()
        .map_err(|e| BootstrapError::Provider(e.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn native_io<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|error| BootstrapError::Io(error.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use zup_runtime::{ExecutionPolicy, OperationControl};

    #[test]
    fn deadlines_leave_native_mutations_alive_and_recovery_waits_for_completion() {
        let scratch = tempfile::tempdir().unwrap();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = PrivateDir::open(scratch.path()).unwrap();
        let operation = OperationControl::new(ExecutionPolicy::NonInteractive);
        let control = RuntimeControl::new(
            operation.cancellation(),
            operation.events(),
            operation.policy(),
        );
        let id = PrerequisiteId::new("native-test").unwrap();
        let process = PackageProcess {
            directory: &directory,
            id: &id,
            program: "/bin/sleep",
            control: &control,
            timeout: Duration::from_millis(20),
        };
        let error = process
            .execute(Command::new("/bin/sleep").arg("0.3"))
            .unwrap_err();
        assert!(matches!(error, BootstrapError::RecoveryRequired(_)));
        assert!(matches!(
            process.check_previous(true),
            Err(BootstrapError::RecoveryRequired(_))
        ));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if process.check_previous(true).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "native process did not finish");
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }

    #[test]
    fn launch_failures_are_distinct_from_uncertain_requests_and_diagnostics_survive() {
        let scratch = tempfile::tempdir().unwrap();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = PrivateDir::open(scratch.path()).unwrap();
        let operation = OperationControl::new(ExecutionPolicy::NonInteractive);
        let control = RuntimeControl::new(
            operation.cancellation(),
            operation.events(),
            operation.policy(),
        );
        let id = PrerequisiteId::new("native-test").unwrap();
        let process = PackageProcess {
            directory: &directory,
            id: &id,
            program: "/bin/echo",
            control: &control,
            timeout: Duration::from_secs(3),
        };
        assert!(matches!(
            process.execute(&mut Command::new("/zup-missing-package-manager")),
            Err(BootstrapError::ProviderPreflight(_))
        ));
        assert_eq!(
            process.check_previous(true).unwrap(),
            PreviousProcess::NotStarted
        );
        let (status, diagnostic) = process
            .execute(Command::new("/bin/echo").arg("native diagnostic: ✓"))
            .unwrap();
        assert!(status.success());
        assert_eq!(diagnostic, "native diagnostic: ✓\n");
        assert_eq!(
            process.check_previous(true).unwrap(),
            PreviousProcess::Settled
        );
        process.record(ProcessState::Intended).unwrap();
        assert!(matches!(
            process.check_previous(true),
            Err(BootstrapError::RecoveryRequired(_))
        ));
    }

    #[test]
    fn cancellation_prevents_launch_but_does_not_interrupt_a_native_transaction() {
        let scratch = tempfile::tempdir().unwrap();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = PrivateDir::open(scratch.path()).unwrap();
        let operation = OperationControl::new(ExecutionPolicy::NonInteractive);
        let control = RuntimeControl::new(
            operation.cancellation(),
            operation.events(),
            operation.policy(),
        );
        let id = PrerequisiteId::new("native-test").unwrap();
        let process = PackageProcess {
            directory: &directory,
            id: &id,
            program: "/bin/sleep",
            control: &control,
            timeout: Duration::from_secs(3),
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if let Ok(file) = directory.open_file(format!("{}.json", process.stem()))
                        && let Ok(Evidence {
                            state: ProcessState::Running { .. },
                            ..
                        }) = serde_json::from_reader::<_, Evidence>(file)
                    {
                        control.cancellation.cancel();
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "native launch did not become observable"
                    );
                    std::thread::park_timeout(Duration::from_millis(5));
                }
            });
            let (status, _) = process
                .execute(Command::new("/bin/sleep").arg("0.3"))
                .unwrap();
            assert!(
                status.success(),
                "cancellation must not kill the package process"
            );
        });
        assert!(control.cancellation.is_cancelled());
        assert!(matches!(
            process.execute(&mut Command::new("/bin/false")),
            Err(BootstrapError::ProviderPreflight(_))
        ));
        assert_eq!(
            process.check_previous(true).unwrap(),
            PreviousProcess::Settled
        );
    }
}
