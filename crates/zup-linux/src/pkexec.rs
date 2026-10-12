use crate::error::IpcError;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const PKEXEC_CANDIDATES: &[&str] = &["/usr/bin/pkexec", "/bin/pkexec"];

pub trait PkexecLauncher {
    type Child: WorkerChild;

    fn spawn(&self, executable: &Path, args: &[String]) -> Result<Self::Child, IpcError>;
}

pub trait WorkerChild {
    fn pid(&self) -> u32;

    fn try_wait(&mut self) -> Result<Option<LaunchOutcome>, IpcError>;

    fn wait(self) -> Result<LaunchOutcome, IpcError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub code: Option<i32>,

    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct SystemPkexec {
    path: PathBuf,
}

impl SystemPkexec {
    pub fn resolve() -> Result<Self, IpcError> {
        resolve_pkexec().map(|path| Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PkexecLauncher for SystemPkexec {
    type Child = SystemWorkerChild;

    fn spawn(&self, executable: &Path, args: &[String]) -> Result<SystemWorkerChild, IpcError> {
        std::process::Command::new(&self.path)
            .arg(executable)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map(|child| SystemWorkerChild {
                child,
                collected: None,
            })
            .map_err(|source| IpcError::Spawn {
                path: self.path.display().to_string(),
                source,
            })
    }
}

pub struct SystemWorkerChild {
    child: std::process::Child,

    collected: Option<LaunchOutcome>,
}

impl WorkerChild for SystemWorkerChild {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<LaunchOutcome>, IpcError> {
        if let Some(outcome) = &self.collected {
            return Ok(Some(outcome.clone()));
        }
        match self.child.try_wait().map_err(|source| {
            IpcError::PkexecWorkerFailed(format!("waiting for the worker: {source}"))
        })? {
            None => Ok(None),
            Some(status) => {
                use std::io::Read as _;
                let mut stderr = Vec::new();
                if let Some(pipe) = self.child.stderr.as_mut() {
                    let _ = pipe.read_to_end(&mut stderr);
                }
                let outcome = LaunchOutcome {
                    code: status.code(),
                    stderr: String::from_utf8_lossy(&stderr).chars().take(512).collect(),
                };
                self.collected = Some(outcome.clone());
                Ok(Some(outcome))
            }
        }
    }

    fn wait(mut self) -> Result<LaunchOutcome, IpcError> {
        if let Some(outcome) = self.collected.take() {
            return Ok(outcome);
        }
        let output = self.child.wait_with_output().map_err(|source| {
            IpcError::PkexecWorkerFailed(format!("waiting for the worker: {source}"))
        })?;
        Ok(LaunchOutcome {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(512)
                .collect(),
        })
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FakePkexec {
    pub pid: u32,

    pub outcome: LaunchOutcome,
}

#[cfg(test)]
impl PkexecLauncher for FakePkexec {
    type Child = FakeWorkerChild;

    fn spawn(&self, _executable: &Path, _args: &[String]) -> Result<FakeWorkerChild, IpcError> {
        Ok(FakeWorkerChild {
            pid: self.pid,
            outcome: self.outcome.clone(),
        })
    }
}

#[cfg(test)]
pub struct FakeWorkerChild {
    pid: u32,
    outcome: LaunchOutcome,
}

#[cfg(test)]
impl WorkerChild for FakeWorkerChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<LaunchOutcome>, IpcError> {
        Ok(None)
    }

    fn wait(self) -> Result<LaunchOutcome, IpcError> {
        Ok(self.outcome)
    }
}

pub fn map_launch(outcome: &LaunchOutcome) -> Result<(), IpcError> {
    match outcome.code {
        Some(0) => Ok(()),
        Some(126) => {
            if outcome.stderr.to_lowercase().contains("dismissed") {
                Err(IpcError::PkexecCancelled)
            } else {
                Err(IpcError::AuthorizationFailed(outcome.stderr.clone()))
            }
        }
        Some(127) => Err(IpcError::PkexecUnavailable(outcome.stderr.clone())),
        Some(code) => Err(IpcError::PkexecWorkerFailed(format!(
            "exit {code}: {}",
            outcome.stderr.trim()
        ))),
        None => Err(IpcError::PkexecWorkerFailed(
            "terminated by a signal".to_owned(),
        )),
    }
}

fn resolve_pkexec() -> Result<PathBuf, IpcError> {
    for candidate in PKEXEC_CANDIDATES {
        let path = Path::new(candidate);
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.uid() != 0 {
            continue;
        }
        if metadata.mode() & 0o022 != 0 {
            continue;
        }
        if metadata.mode() & 0o111 == 0 {
            continue;
        }
        return Ok(path.to_path_buf());
    }
    Err(IpcError::PkexecUnavailable(
        "no system-owned pkexec executable found".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_maps_to_success() {
        let outcome = LaunchOutcome {
            code: Some(0),
            stderr: String::new(),
        };
        assert!(map_launch(&outcome).is_ok());
    }

    #[test]
    fn dismissal_maps_to_cancelled() {
        let outcome = LaunchOutcome {
            code: Some(126),
            stderr: "Error executing command as another user: User dismissed authentication dialog"
                .into(),
        };
        assert!(matches!(
            map_launch(&outcome),
            Err(IpcError::PkexecCancelled)
        ));
    }

    #[test]
    fn denial_maps_to_authorization_failed_not_cancelled() {
        let outcome = LaunchOutcome {
            code: Some(126),
            stderr: "Error executing command as another user: Not authorized".into(),
        };
        assert!(matches!(
            map_launch(&outcome),
            Err(IpcError::AuthorizationFailed(_))
        ));
    }

    #[test]
    fn missing_mechanism_maps_to_unavailable() {
        let outcome = LaunchOutcome {
            code: Some(127),
            stderr: "command not found".into(),
        };
        assert!(matches!(
            map_launch(&outcome),
            Err(IpcError::PkexecUnavailable(_))
        ));
    }

    #[test]
    fn worker_failure_keeps_its_code() {
        let outcome = LaunchOutcome {
            code: Some(1),
            stderr: "policy refusal".into(),
        };
        let error = map_launch(&outcome).expect_err("a worker failure");
        assert!(error.to_string().contains("exit 1"), "{error}");
    }

    #[test]
    fn a_signal_is_a_worker_failure_not_silence() {
        let outcome = LaunchOutcome {
            code: None,
            stderr: String::new(),
        };
        assert!(matches!(
            map_launch(&outcome),
            Err(IpcError::PkexecWorkerFailed(_))
        ));
    }

    #[test]
    fn a_fake_launcher_reports_its_pid_and_outcome() {
        let launcher = FakePkexec {
            pid: 4242,
            outcome: LaunchOutcome {
                code: Some(0),
                stderr: String::new(),
            },
        };
        let child = launcher
            .spawn(Path::new("/bin/true"), &[])
            .expect("a fake spawn");
        assert_eq!(child.pid(), 4242);
        let outcome = child.wait().expect("a fake wait");
        assert!(map_launch(&outcome).is_ok());

        let denied = FakePkexec {
            pid: 4243,
            outcome: LaunchOutcome {
                code: Some(126),
                stderr: "Not authorized".into(),
            },
        };
        let outcome = denied
            .spawn(Path::new("/bin/true"), &[])
            .expect("a fake spawn")
            .wait()
            .expect("a fake wait");
        assert!(matches!(
            map_launch(&outcome),
            Err(IpcError::AuthorizationFailed(_))
        ));
    }

    #[test]
    fn resolution_names_an_absolute_candidate_or_is_unavailable() {
        match resolve_pkexec() {
            Ok(path) => {
                assert!(path.is_absolute(), "{}", path.display());
                assert!(
                    PKEXEC_CANDIDATES.contains(&path.to_str().unwrap_or_default()),
                    "{}",
                    path.display()
                );
            }
            Err(error) => {
                assert!(
                    !Path::new("/usr/bin/pkexec").exists() && !Path::new("/bin/pkexec").exists(),
                    "a present system pkexec must resolve: {error}"
                );
            }
        }
    }
}
