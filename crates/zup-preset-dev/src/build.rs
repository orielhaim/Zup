use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Stdio};

use cargo_metadata::Message;
use cargo_metadata::diagnostic::DiagnosticLevel;
use process_wrap::std::{ChildWrapper, CommandWrap};

use crate::project::Project;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: String,
    pub message: String,
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

#[derive(Debug)]
pub enum Build {
    Succeeded { executable: PathBuf },
    Failed { diagnostics: Vec<Diagnostic> },
    Unusable(String),
}

impl Build {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }
}

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

    pub fn binary(&self) -> &str {
        &self.project.binary
    }

    pub fn start(&self, cargo: &Path) -> Result<(ChildStdout, Building), String> {
        let mut command = CommandWrap::with_new(cargo, |command| {
            command
                .current_dir(&self.project.root)
                .args([
                    "build",
                    "--package",
                    &self.project.name,
                    "--bin",
                    &self.project.binary,
                    "--profile",
                    &self.profile,
                    "--message-format=json",
                ])
                .stdin(Stdio::null())
                .stderr(Stdio::inherit())
                .stdout(Stdio::piped());
        });
        manage(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not run `{}`: {error}", cargo.display()))?;
        let out = child.stdout().take().expect("a piped build has a pipe");
        Ok((out, Building { child }))
    }

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

#[derive(Debug)]
pub struct Building {
    child: Box<dyn ChildWrapper>,
}

impl Building {
    pub fn wait(mut self) {
        let _ = self.child.wait();
    }
}

impl Drop for Building {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn manage(command: &mut CommandWrap) {
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
}
