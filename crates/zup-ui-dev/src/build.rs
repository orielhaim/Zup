//! Compiling a preset, and reading the compiler's own account of it.
//!
//! Cargo is the build system. This runs it, and reads what it says through
//! `--message-format=json`, which is a machine contract rather than a rendering:
//! every diagnostic arrives as the compiler emitted it, with a file, a line, and
//! a span, and every artifact arrives with the exact path it will exist at.
//!
//! The exact path matters. A development environment runs the copy it is about
//! to launch, and that copy has to be a file this process may delete and rewrite
//! while an earlier one is still open somewhere else. Guessing where Cargo put
//! something is how a watcher ends up executing a previous build's output, which
//! is the kind of bug that reproduces once a week and is never the watcher.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use cargo_metadata::Message;
use cargo_metadata::diagnostic::DiagnosticLevel;

use crate::project::Project;

/// One thing that went wrong in a build, as a compiler said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What a compiler calls the thing that failed: error, and one day warnings.
    pub level: String,
    /// The message, without the file and line a terminal would have put in front.
    pub message: String,
    /// Where, as the compiler named it.
    pub where_: Option<String>,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.where_ {
            Some(where_) => write!(out, "{where_}: {} {}", self.level, self.message),
            None => write!(out, "{}: {}", self.level, self.message),
        }
    }
}

/// What one build produced.
#[derive(Debug)]
pub enum Build {
    /// It compiled, and this is the executable it produced.
    Succeeded { executable: PathBuf },
    /// It did not. The previous run is untouched.
    Failed { diagnostics: Vec<Diagnostic> },
    /// Cargo itself could not be run, or said something that is not a build.
    Unusable(String),
}

impl Build {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }
}

/// Compiles a preset, one at a time.
///
/// One at a time, deliberately. Two concurrent builds of one project fight over
/// the same target directory, and the loser is whichever one Cargo decides to
/// stop, which is not a decision this tool should be making on a preset author's
/// behalf while they are typing.
#[derive(Debug)]
pub struct Supervisor {
    project: Project,
    profile: String,
}

impl Supervisor {
    pub fn new(project: Project, profile: &str) -> Self {
        Self {
            project,
            profile: profile.to_owned(),
        }
    }

    /// The binary this supervisor builds.
    pub fn binary(&self) -> &str {
        &self.project.binary
    }

    /// Start a build, handing back its output pipe and the process itself.
    ///
    /// Both belong to whoever reads the result. A supervisor that keeps a handle
    /// it has to remember to poll is how a second build silently never starts:
    /// the first handle is still there, the supervisor believes a build is
    /// running, and nothing ever replaces it. The thread reading the pipe is the
    /// only thing that can know a build is over, so it is the thing that holds
    /// the process and waits on it.
    pub fn start(
        &self,
        cargo: &std::path::Path,
    ) -> Result<(std::process::ChildStdout, Child), String> {
        let mut child = Command::new(cargo)
            .current_dir(&self.project.root)
            .args([
                "build",
                "--package",
                &self.project.name,
                "--bin",
                &self.project.binary,
                "--profile",
                &self.profile,
                // `json` rather than `json-render-diagnostics`: cargo emits the
                // rendered form beside the machine form on this channel, and the
                // machine form is the one that carries every message. Standard
                // error is inherited, so a person still sees cargo's own
                // rendering as well.
                "--message-format=json",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run `{}`: {error}", cargo.display()))?;
        let out = child.stdout.take().expect("a piped build has a pipe");
        Ok((out, child))
    }

    /// Read a build's result from a running process's output pipe.
    ///
    /// Blocking, and on a thread of its own: a build is the slowest thing this
    /// tool does, and the watcher, the running preset, and the controls a person
    /// is using must not wait for it.
    pub fn read(reader: impl BufRead, wanted: &str) -> Build {
        let mut built: Option<PathBuf> = None;
        let mut diagnostics = Vec::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Message>(&line) else {
                continue;
            };
            match message {
                Message::CompilerMessage(message) => {
                    if message.message.level == DiagnosticLevel::Error {
                        diagnostics.push(Diagnostic {
                            level: "error".into(),
                            message: message.message.message.clone(),
                            where_: message
                                .message
                                .spans
                                .iter()
                                .find(|span| span.is_primary)
                                .map(|span| format!("{}:{}", span.file_name, span.line_start + 1))
                                .or_else(|| Some(message.target.name.clone())),
                        });
                    }
                }
                Message::CompilerArtifact(artifact) => {
                    // A build emits an artifact per compilation unit, and only one
                    // of them is the preset. Cargo names it, and the name in the
                    // artifact is the one the manifest declared.
                    if artifact
                        .target
                        .kind
                        .iter()
                        .any(|kind| kind.to_string() == "bin")
                        && artifact.target.name == wanted
                        && let Some(path) = artifact.executable.as_ref()
                    {
                        built = Some(PathBuf::from(path.as_std_path()));
                    }
                }
                Message::BuildFinished(finished) => {
                    if finished.success {
                        return match built {
                            Some(executable) => Build::Succeeded { executable },
                            // Cargo reported success without naming the binary.
                            // Running a guessed path is how a watcher ends up
                            // executing the previous build's output.
                            None => Build::Unusable(format!(
                                "cargo reported a successful build without producing `{wanted}`"
                            )),
                        };
                    }
                    return Build::Failed { diagnostics };
                }
                _ => {}
            }
        }
        Build::Unusable("the build ended without reporting a result".into())
    }
}
