//! Launching the privileged worker through `pkexec`.
//!
//! The production architecture uses `pkexec` directly: no sudo wrapper crate,
//! no setuid binary, no file capabilities, no long-running root daemon. An
//! unprivileged caller that needs a machine mutation runs `pkexec` on the
//! installer binary itself in a hidden worker mode; polkit authenticates the
//! administrator, and Zup never requests, reads, proxies, or stores any
//! password.
//!
//! # Resolution
//!
//! The system `pkexec` is resolved deliberately from absolute candidate
//! paths, never from an attacker-controlled `PATH`. The resolved executable
//! must be a regular system-owned file that the invoking user cannot write.
//!
//! # Testability
//!
//! Launching is behind [`PkexecLauncher`]: production uses
//! [`SystemPkexec`], tests inject a fake. The resolution itself is never
//! injectable through environment variables - an untrusted runtime request
//! must not be able to redirect elevation.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Absolute locations a system `pkexec` lives in, in preference order.
const PKEXEC_CANDIDATES: &[&str] = &["/usr/bin/pkexec", "/bin/pkexec"];

/// Why elevation could not run the worker.
#[derive(Debug, thiserror::Error)]
pub enum PkexecError {
    /// No usable system `pkexec` exists.
    #[error("pkexec is unavailable: {0}")]
    Unavailable(String),

    /// The administrator authentication did not happen: the user cancelled.
    #[error("administrator authentication was cancelled")]
    Cancelled,

    /// The administrator authentication happened and refused.
    #[error("administrator authorization failed: {0}")]
    AuthorizationFailed(String),

    /// The worker ran but reported failure after authorization.
    #[error("privileged worker failed: {0}")]
    WorkerFailed(String),

    /// The worker channel broke after authorization.
    #[error("privileged worker protocol failed: {0}")]
    Protocol(String),

    /// `pkexec` itself could not be started.
    #[error("could not start pkexec at `{path}`: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// How to start the worker process. Production resolves the system `pkexec`;
/// tests inject a fake. The resolution itself is never injectable through
/// environment variables - an untrusted runtime request must not be able
/// to redirect elevation.
pub trait PkexecLauncher {
    /// The running worker child.
    type Child: WorkerChild;

    /// Start `executable` with `args` through the elevation mechanism.
    ///
    /// The child keeps its pipes: the client's handshake runs while the
    /// worker lives, and [`WorkerChild::wait`] collects the ending after
    /// the protocol completes.
    fn spawn(&self, executable: &Path, args: &[String]) -> Result<Self::Child, PkexecError>;
}

/// A running worker child: a pid to verify, and an ending to collect.
pub trait WorkerChild {
    /// The child's process id, for peer verification.
    fn pid(&self) -> u32;

    /// Wait for the ending and collect the outcome.
    fn wait(self) -> Result<LaunchOutcome, PkexecError>;
}

/// What an elevation attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// The worker process exit status code, if it ran.
    pub code: Option<i32>,
    /// Anything the mechanism wrote to stderr, truncated.
    pub stderr: String,
}

/// The production launcher: the resolved system `pkexec`.
#[derive(Debug, Clone)]
pub struct SystemPkexec {
    path: PathBuf,
}

impl SystemPkexec {
    /// Resolve the system `pkexec`, strictly.
    pub fn resolve() -> Result<Self, PkexecError> {
        resolve_pkexec().map(|path| Self { path })
    }

    /// The resolved executable, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PkexecLauncher for SystemPkexec {
    type Child = SystemWorkerChild;

    fn spawn(&self, executable: &Path, args: &[String]) -> Result<SystemWorkerChild, PkexecError> {
        // Piped, not inherited: the protocol carries progress and results,
        // and stderr is captured for the typed result mapping. Polkit
        // authenticates through its own agent (graphical, or the internal
        // text agent on this terminal) - never through these pipes - so Zup
        // still never sees a password.
        std::process::Command::new(&self.path)
            .arg(executable)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map(|child| SystemWorkerChild { child })
            .map_err(|source| PkexecError::Spawn {
                path: self.path.display().to_string(),
                source,
            })
    }
}

/// The production worker child.
pub struct SystemWorkerChild {
    child: std::process::Child,
}

impl WorkerChild for SystemWorkerChild {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait(self) -> Result<LaunchOutcome, PkexecError> {
        let output = self.child.wait_with_output().map_err(|source| {
            PkexecError::WorkerFailed(format!("waiting for the worker: {source}"))
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

/// A fake launcher for tests: canned pid and outcome, no process.
///
/// Test-only dependency injection for result mapping: production resolution
/// stays strict and never consults the environment.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct FakePkexec {
    /// The pid the fake child reports.
    pub pid: u32,
    /// The outcome the fake child ends with.
    pub outcome: LaunchOutcome,
}

#[cfg(any(test, feature = "test-support"))]
impl PkexecLauncher for FakePkexec {
    type Child = FakeWorkerChild;

    fn spawn(&self, _executable: &Path, _args: &[String]) -> Result<FakeWorkerChild, PkexecError> {
        Ok(FakeWorkerChild {
            pid: self.pid,
            outcome: self.outcome.clone(),
        })
    }
}

/// A fake worker child for tests.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeWorkerChild {
    pid: u32,
    outcome: LaunchOutcome,
}

#[cfg(any(test, feature = "test-support"))]
impl WorkerChild for FakeWorkerChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn wait(self) -> Result<LaunchOutcome, PkexecError> {
        Ok(self.outcome)
    }
}

/// Map a launch outcome onto the typed elevation result.
///
/// `pkexec` exit codes: `0` means the worker ran (its own report decides);
/// `126` means authorization was not granted - a dismissal when polkit says
/// so, a denial otherwise; `127` means something on the way there is missing
/// or broken. Anything else is the worker failing after authorization.
pub fn map_launch(outcome: &LaunchOutcome) -> Result<(), PkexecError> {
    match outcome.code {
        Some(0) => Ok(()),
        Some(126) => {
            if outcome.stderr.to_lowercase().contains("dismissed") {
                Err(PkexecError::Cancelled)
            } else {
                Err(PkexecError::AuthorizationFailed(outcome.stderr.clone()))
            }
        }
        Some(127) => Err(PkexecError::Unavailable(outcome.stderr.clone())),
        Some(code) => Err(PkexecError::WorkerFailed(format!(
            "exit {code}: {}",
            outcome.stderr.trim()
        ))),
        None => Err(PkexecError::WorkerFailed(
            "terminated by a signal".to_owned(),
        )),
    }
}

/// Resolve a usable system `pkexec` from the absolute candidates.
///
/// Each candidate is validated: it must exist as a regular executable file
/// owned by root and writable by nobody but root. A candidate that fails
/// validation does not poison the rest - `/bin` is commonly a symlink to
/// `/usr/bin` - but when nothing validates, elevation is unavailable rather
/// than attempted through a binary nobody checked.
fn resolve_pkexec() -> Result<PathBuf, PkexecError> {
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
    Err(PkexecError::Unavailable(
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
        assert!(matches!(map_launch(&outcome), Err(PkexecError::Cancelled)));
    }

    #[test]
    fn denial_maps_to_authorization_failed_not_cancelled() {
        let outcome = LaunchOutcome {
            code: Some(126),
            stderr: "Error executing command as another user: Not authorized".into(),
        };
        assert!(matches!(
            map_launch(&outcome),
            Err(PkexecError::AuthorizationFailed(_))
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
            Err(PkexecError::Unavailable(_))
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
            Err(PkexecError::WorkerFailed(_))
        ));
    }

    #[test]
    fn resolution_names_an_absolute_candidate_or_is_unavailable() {
        // The candidates are absolute system paths; when none validates, the
        // answer is unavailable, never a PATH search.
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
