//! The development session: build, watch, run, and replace.
//!
//! The machine, the child, the controls and the loop are `zup-preview`'s. What
//! is here is the half that only a preset author needs: a compiler to supervise,
//! and the rule that a source change is a build while a document or a file the
//! document names is not.
//!
//! That rule is the reason a settings change feels immediate, and it is also the
//! reason the build is the expensive path. Everything expensive in this tool goes
//! through [`Session::start_build`], and nothing else starts one: a change that
//! does not compile leaves the running window alone, and a change that does
//! compile replaces it only once the new child has opened its session.
//!
//! Both children this session owns - the compiler and the preset it built - are
//! launched managed, and their termination is this crate's decision rather than
//! `process-wrap`'s. A build that is still running when a session ends is ended
//! with it, and a preset is replaced only after its successor has proved it can
//! start.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

use zup_core::UiPreset;
use zup_preview::{Driver, Event, Runtime, StateDirectory, default_scenario, serve};

use crate::build::{Build, Supervisor};
use crate::development::{Development, DevelopmentError};
use crate::project::Project;
use crate::watch::{Change, Watched, Watcher};

/// What a person asked for.
#[derive(Debug, Clone)]
pub struct Request {
    /// The preset project to develop.
    pub root: PathBuf,
    /// The Cargo profile to build with.
    pub profile: String,
}

/// Why a development session could not run.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not read the preset project: {0}")]
    Project(#[from] crate::project::ProjectError),
    #[error("could not read the development document: {0}")]
    Development(#[from] DevelopmentError),
    #[error("could not watch for changes: {0}")]
    Watch(String),
    #[error("could not run cargo: {0}")]
    Cargo(String),
}

/// The development session.
pub struct Session {
    runtime: Runtime,
    project: Project,
    development: Development,
    supervisor: Supervisor,
    /// Whether a build is wanted but not yet started.
    queued: bool,
    /// The settings schema the current preset generates, once one has been built.
    schema: Option<serde_json::Value>,
    events: Sender<Event<Change, Build>>,
    cargo: PathBuf,
}

impl Session {
    /// Open a development session and run it until somebody quits.
    pub fn run(request: Request) -> Result<(), SessionError> {
        let project = Project::read(&request.root)?;
        let development = Development::read(&project.root)?;
        let (events, inbox) = Event::channel();

        let watching = events.clone();
        let watched = project.clone();
        let watched_development = development.clone();
        std::thread::Builder::new()
            .name("zup-preset-dev-watch".into())
            .spawn(
                move || match Watcher::start(&watched, &watched_development) {
                    Ok(mut watcher) => {
                        while let Some(seen) = watcher.next_change() {
                            let event = match seen {
                                Watched::Changed(change) => Event::Changed(change),
                                // Said rather than swallowed. A session that has
                                // stopped watching must not look like a session in
                                // which nothing needed rebuilding.
                                Watched::Failed(reason) => {
                                    Event::Report(format!("watching: {reason}"))
                                }
                            };
                            if watching.send(event).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = watching.send(Event::Report(error));
                    }
                },
            )
            .map_err(|error| SessionError::Watch(error.to_string()))?;

        let mut session = Self {
            supervisor: Supervisor::new(project.clone(), &request.profile),
            runtime: Runtime::new(
                StateDirectory::under(&project.root, "dev"),
                default_scenario(),
            ),
            project,
            development,
            queued: true,
            schema: None,
            events,
            cargo: cargo_executable(),
        };
        Event::attach_terminal(session.events.clone()).map_err(SessionError::Watch)?;
        serve(&inbox, &mut session);
        Ok(())
    }

    fn start_build(&mut self) {
        self.queued = false;
        // Said before the process starts, because a preset's first build
        // compiles the whole GPUI stack and takes minutes, and a session that
        // has said nothing for two of them looks like one that has hung.
        println!("  build    {}", self.supervisor.binary());
        let (out, build) = match self.supervisor.start(&self.cargo) {
            Ok(started) => started,
            Err(error) => {
                println!("  build    {error}");
                return;
            }
        };
        let reporting = self.events.clone();
        let binary = self.supervisor.binary().to_owned();
        std::thread::Builder::new()
            .name("zup-preset-dev-build".into())
            .spawn(move || {
                let _ = reporting.send(Event::Finished(Supervisor::read(
                    BufReader::new(out),
                    &binary,
                )));
                // The build is this thread's to finish with, which is what lets the
                // next build start rather than finding a tree nobody ended.
                build.wait();
            })
            .expect("a build reader thread");
    }

    fn built(&mut self, build: Build) {
        match build {
            Build::Succeeded { executable } => self.replace(executable),
            Build::Failed { diagnostics } => {
                // The window stays. A preset that no longer compiles is a fact
                // about the source, not about the process already running the
                // last version that did.
                println!("  build    did not compile; the previous preset is still running");
                for diagnostic in diagnostics {
                    println!("    {diagnostic}");
                }
            }
            Build::Unusable(reason) => println!("  build    {reason}"),
        }
    }

    /// Start a new child and, only once it has opened the session, replace the
    /// one that is running.
    fn replace(&mut self, executable: PathBuf) {
        let description = match self.describe(&executable) {
            Ok(description) => description,
            Err(reason) => {
                println!("  build    {reason}");
                return;
            }
        };
        // The preset's own account of itself is the only thing that says what it
        // needs, and the SDK wrote it. A compatibility failure here is the same
        // refusal a build would give, arrived at from the other side.
        let preset = UiPreset {
            name: zup_core::NonEmptyString::new(description.name.clone()).expect("a named preset"),
            version: semver::Version::parse(&description.version)
                .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
            protocol: description.ui_protocol,
            required_capabilities: description
                .required_capabilities
                .names()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            // The configuration the child receives is the session's own, and it
            // is set below from the development document. Only the protocol and
            // the required capabilities are read before the child is launched.
            settings: serde_json::Value::Null,
            assets: Vec::new(),
        };
        self.schema = Some(description.settings_schema.clone());
        let bytes = match std::fs::read(&executable) {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("  build    {}: {error}", executable.display());
                return;
            }
        };
        if let Err(error) = self.runtime.present(&preset, &bytes) {
            println!("  build    {error}; the previous preset is still running");
            return;
        }
        self.reload_configuration();
    }

    /// Re-read the document, and give the preset what it now says.
    fn reload_configuration(&mut self) {
        self.runtime.clear_assets();
        let files = match self.development.asset_files(&self.project.root) {
            Ok(files) => files,
            Err(error) => {
                println!("  {error}");
                return;
            }
        };
        for (name, path) in files {
            if let Err(error) = self.runtime.set_asset(&name, &path) {
                println!("  asset    {error}");
            }
        }
        let Some(schema) = self.schema.clone() else {
            return;
        };
        // Validated here rather than in the preset, because the schema is on this
        // side of the process boundary and the preset is a program that is about
        // to be launched. A document that does not fit leaves the last one in
        // force, which is the point of checking: a typo must not empty a window
        // that is working.
        let problems = validator(&schema)(&self.development.settings);
        if !problems.is_empty() {
            for problem in problems {
                println!("  settings {problem}");
            }
            return;
        }
        let settings = self.development.settings.clone();
        self.runtime.set_settings(settings);
        println!("  settings the application changed");
    }

    fn changed(&mut self, change: Change) {
        match change {
            // The document and the files it names are data. Rebuilding for them
            // would spend the slowest thing this tool does on a comma, and the
            // whole reason a settings change feels immediate is that it does not.
            Change::Configuration | Change::Asset(_) => match Development::read(&self.project.root)
            {
                Ok(development) => self.development = development,
                Err(error) => {
                    println!("  {error}");
                    return;
                }
            },
            Change::Source => {
                println!("  changed  rebuilding");
                self.queued = true;
            }
        }
        if !matches!(change, Change::Source) {
            self.reload_configuration();
        }
    }

    /// What the preset says it is, read the way `zup ui pack` reads it.
    ///
    /// The same document, produced by the same code, so a preset that cannot be
    /// developed against is refused for the reason a build would refuse it
    /// rather than for a new one.
    fn describe(&self, executable: &Path) -> Result<zup_preset_protocol::PresetDescription, String> {
        let output = Command::new(executable)
            .arg(zup_preset_protocol::DESCRIBE_FLAG)
            .output()
            .map_err(|error| format!("could not run `{}`: {error}", executable.display()))?;
        if !output.status.success() {
            return Err(format!(
                "the preset's describe mode failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if output.stdout.len() > zup_preset_protocol::MAX_DESCRIBE_BYTES {
            return Err(format!(
                "the preset described itself in {} bytes; the limit is {}",
                output.stdout.len(),
                zup_preset_protocol::MAX_DESCRIBE_BYTES
            ));
        }
        let description: zup_preset_protocol::PresetDescription =
            serde_json::from_slice(&output.stdout)
                .map_err(|error| format!("the preset's describe document is unusable: {error}"))?;
        description
            .validate()
            .map_err(|error| format!("the preset described itself as unusable: {error}"))?;
        Ok(description)
    }
}

impl Driver for Session {
    type Change = Change;
    type Finished = Build;

    fn runtime(&mut self) -> &mut Runtime {
        &mut self.runtime
    }

    fn tick(&mut self) {
        if self.queued {
            self.start_build();
        }
    }

    fn changed(&mut self, change: Change) {
        Session::changed(self, change);
    }

    fn finished(&mut self, outcome: Build) {
        self.built(outcome);
    }
}

/// The settings a preset accepts, as a check that needs no preset process.
///
/// The same offline validator the build uses, and for the same reason: a
/// development document that does not fit is refused before anything is launched
/// rather than by a program that has already opened a window.
fn validator(schema: &serde_json::Value) -> impl Fn(&serde_json::Value) -> Vec<String> + use<'_> {
    let compiled = jsonschema::options()
        .offline()
        .should_validate_formats(false)
        .build(schema)
        .ok();
    move |settings: &serde_json::Value| match compiled.as_ref() {
        Some(validator) => validator
            .iter_errors(settings)
            .map(|error| error.to_string())
            .collect(),
        None => vec!["this preset's settings schema is not a schema this build can use".into()],
    }
}

fn cargo_executable() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}
