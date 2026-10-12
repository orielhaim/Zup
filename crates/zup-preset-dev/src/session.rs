use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

use zup_core::PresetRuntime;
use zup_preview::{Driver, Event, Runtime, StateDirectory, default_scenario, serve};

use crate::build::{Build, Supervisor};
use crate::development::{Development, DevelopmentError};
use crate::project::Project;
use crate::watch::{Change, Watched, Watcher};

#[derive(Debug, Clone)]
pub struct Request {
    pub root: PathBuf,
    pub profile: String,
}

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

pub struct Session {
    runtime: Runtime,
    project: Project,
    development: Development,
    supervisor: Supervisor,
    queued: bool,
    schema: Option<serde_json::Value>,
    events: Sender<Event<Change, Build>>,
    cargo: PathBuf,
}

impl Session {
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
                build.wait();
            })
            .expect("a build reader thread");
    }

    fn built(&mut self, build: Build) {
        match build {
            Build::Succeeded { executable } => self.replace(executable),
            Build::Failed { diagnostics } => {
                println!("  build    did not compile; the previous preset is still running");
                for diagnostic in diagnostics {
                    println!("    {diagnostic}");
                }
            }
            Build::Unusable(reason) => println!("  build    {reason}"),
        }
    }

    fn replace(&mut self, executable: PathBuf) {
        let description = match self.describe(&executable) {
            Ok(description) => description,
            Err(reason) => {
                println!("  build    {reason}");
                return;
            }
        };
        let preset = PresetRuntime {
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

    fn describe(
        &self,
        executable: &Path,
    ) -> Result<zup_preset_protocol::PresetDescription, String> {
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
