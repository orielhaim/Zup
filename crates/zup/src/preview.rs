use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use clap::{Args, ValueHint};
use zup_core::{Frontend, Installer, Sha256Digest, TargetTriple, hash_bytes};
use zup_preset_compose::{PresetProblem, Resolved};
use zup_preview::{
    ControlOutcome, Driver, Event, Runtime, Scenario, Seen, StateDirectory, Watcher, serve,
};
use zup_toolchain::ToolchainComponent;

use crate::failure;
use crate::project::{SelectedProject, TargetOverrideArgs};
use crate::toolchain::ToolchainResolver;

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("{0}")]
    Project(String),
    #[error("this application presents no window: {reason}")]
    NoWindow { reason: String },
    #[error("{0}")]
    Window(#[from] PresetProblem),
    #[error("the window could not be shown: {0}")]
    Start(String),
}

#[derive(Debug, Args)]
pub struct PreviewCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
}

pub fn run(args: PreviewCommand, toolchain: Option<PathBuf>) -> miette::Result<()> {
    let _lines = std::io::LineWriter::new(std::io::stdout());
    let manifest = find_manifest(&args.manifest)?;
    let (events, inbox) = Event::channel();
    let mut session = Session::open(manifest, args.target, Arc::new(crate::resolver(toolchain)?))
        .map_err(|error| failure::error("zup.preview", error.to_string()))?;
    session.watch(events.clone());
    Event::attach_terminal(events)
        .map_err(|error| miette::miette!("could not read the control surface: {error}"))?;
    serve(&inbox, &mut session);
    Ok(())
}

fn find_manifest(named: &Path) -> miette::Result<PathBuf> {
    if named.is_dir() {
        return search_upwards(named);
    }
    if named.is_file() {
        return Ok(named.to_path_buf());
    }
    if named
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return search_upwards(Path::new("."));
    }
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Err(miette::miette!(
        "`{}` is not a file and not a directory, and no project was found in `{}` or above it",
        named.display(),
        here.display()
    ))
}

fn search_upwards(start: &Path) -> miette::Result<PathBuf> {
    let begin = std::fs::canonicalize(start)
        .map_err(|error| miette::miette!("`{}`: {error}", start.display()))?;
    for directory in begin.ancestors() {
        let candidate = directory.join(crate::DEFAULT_MANIFEST);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(miette::miette!(
        "no `{}` in `{}` or any directory above it; `zup preview` presents an application, and \
         this is not one. Run it inside a zup project, or pass --manifest",
        crate::DEFAULT_MANIFEST,
        begin.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Configuration,
    Asset,
    Package,
}

fn classify(
    paths: &BTreeSet<PathBuf>,
    manifest: &Path,
    package: Option<&Path>,
    assets: &[PathBuf],
) -> Option<Change> {
    let mut asset = false;
    let mut selected = false;
    let mut configuration = false;
    for path in paths {
        if path == manifest {
            configuration = true;
        } else if package.is_some_and(|chosen| chosen == path) {
            selected = true;
        } else if assets.iter().any(|file| file == path) {
            asset = true;
        }
    }
    if configuration {
        Some(Change::Configuration)
    } else if asset {
        Some(Change::Asset)
    } else {
        selected.then_some(Change::Package)
    }
}

pub struct Session {
    runtime: Runtime,
    resolver: Arc<ToolchainResolver>,
    manifest: PathBuf,
    targets: Vec<String>,
    project_root: PathBuf,
    state: StateDirectory,
    presented: Option<Sha256Digest>,
    package: Option<PathBuf>,
    assets: Vec<PathBuf>,
    queued: bool,
}

impl Session {
    pub fn open(
        manifest: PathBuf,
        targets: Vec<String>,
        resolver: Arc<ToolchainResolver>,
    ) -> Result<Self, PreviewError> {
        let selected = select(&manifest, &targets)?;
        let project_root = zup_build::project_root(&selected.manifest_path);
        let (installer, target) = the_window(&selected)?;
        let state = StateDirectory::under(&project_root, "preview");
        let mut session = Self {
            runtime: Runtime::new(state.clone(), Scenario::from_installer(&installer)),
            resolver,
            manifest: selected.manifest_path.clone(),
            targets,
            project_root,
            state,
            presented: None,
            package: None,
            assets: Vec::new(),
            queued: false,
        };
        let resolved = session.resolve(&installer, &target)?;
        session.adopt(&resolved)?;
        Ok(session)
    }

    pub fn watch(&self, events: Sender<Event<BTreeSet<PathBuf>, ()>>) {
        let root = self.project_root.clone();
        let state = self.state.clone();
        std::thread::Builder::new()
            .name("zup-preview-watch".into())
            .spawn(move || {
                let mut watcher = match Watcher::start(&root, state) {
                    Ok(watcher) => watcher,
                    Err(reason) => {
                        let _ = events.send(Event::Report(format!("watching: {reason}")));
                        return;
                    }
                };
                while let Some(seen) = watcher.next_change() {
                    let event = match seen {
                        Seen::Changed(paths) => Event::Changed(paths),
                        // watching must not look like one in which nothing
                        Seen::Failed(reason) => Event::Report(format!("watching: {reason}")),
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
            })
            .expect("a watch thread");
    }

    pub fn resolve(
        &self,
        installer: &Installer,
        target: &TargetTriple,
    ) -> Result<Resolved, PreviewError> {
        let selected = select(&self.manifest, &self.targets)?;
        let shipped = || {
            self.resolver
                .resolve(&ToolchainComponent::Preset, None)
                .map(|resolved| resolved.path)
                .map_err(|error| error.to_string())
        };
        Ok(zup_preset_compose::resolve(
            &selected.manifest.ui,
            &self.project_root,
            installer,
            target,
            &shipped,
            crate::project::source_policy_for(target),
        )?)
    }

    pub fn adopt(&mut self, resolved: &Resolved) -> Result<(), PreviewError> {
        self.runtime.clear_assets();
        let mut files = Vec::with_capacity(resolved.assets.len());
        for asset in &resolved.assets {
            let Some(source) = asset.source.as_deref() else {
                continue;
            };
            let name = asset.name.to_string();
            let digest = self
                .runtime
                .set_asset(&name, source)
                .map_err(PreviewError::Project)?;
            if digest != asset.sha256 {
                return Err(PreviewError::Project(format!(
                    "`{}` changed while it was being resolved",
                    name
                )));
            }
            files.push(source.to_path_buf());
        }
        self.assets = files;
        self.package = Some(resolved.package.clone());
        let settings = resolved.runtime.settings.clone();
        self.runtime.set_settings(settings);

        let digest = hash_bytes(&resolved.executable);
        if self.presented == Some(digest) {
            self.runtime.publish();
            return Ok(());
        }
        self.runtime
            .present(&resolved.runtime, &resolved.executable)
            .map_err(|error| PreviewError::Start(error.to_string()))?;
        self.presented = Some(digest);
        Ok(())
    }

    pub fn refresh(&mut self) {
        let selected = match select(&self.manifest, &self.targets) {
            Ok(selected) => selected,
            // person does while editing one, and it must not cost the window that
            Err(error) => {
                println!("  manifest {error}");
                return;
            }
        };
        let (installer, target) = match the_window(&selected) {
            Ok(window) => window,
            Err(error) => {
                println!("  window   {error}");
                return;
            }
        };
        let resolved = match self.resolve(&installer, &target) {
            Ok(resolved) => resolved,
            Err(error) => {
                println!("  window   {error}");
                return;
            }
        };
        let window_changed = self
            .presented
            .is_some_and(|presented| presented != hash_bytes(&resolved.executable));
        match self.adopt(&resolved) {
            Ok(()) if window_changed => println!("  window   the application selected a new one"),
            Ok(()) => println!("  settings the application changed"),
            Err(error) => println!("  window   {error}; the last valid preview is still running"),
        }
    }

    pub fn pump(&mut self) {
        Driver::pump(self);
        self.tick();
    }

    pub fn closed(&self) -> bool {
        self.runtime.closed()
    }

    pub fn state(&self) -> &zup_preset_protocol::Snapshot {
        self.runtime.snapshot()
    }

    pub fn configuration(&self) -> &zup_preset_protocol::Configuration {
        self.runtime.configuration()
    }

    pub fn presented(&self) -> Option<Sha256Digest> {
        self.presented
    }

    pub fn package(&self) -> Option<&Path> {
        self.package.as_deref()
    }

    pub fn assets(&self) -> &[PathBuf] {
        &self.assets
    }

    pub fn meaning(&self, paths: &BTreeSet<PathBuf>) -> Option<&'static str> {
        classify(paths, &self.manifest, self.package.as_deref(), &self.assets).map(|change| {
            match change {
                Change::Configuration => "the application",
                Change::Asset => "an asset",
                Change::Package => "the package",
            }
        })
    }

    pub fn control(&mut self, line: &str) -> ControlOutcome {
        self.runtime.control(line)
    }

    pub fn saw_change(&mut self, paths: BTreeSet<PathBuf>) {
        Driver::changed(self, paths);
    }

    pub fn end(&mut self) {
        self.runtime.shutdown();
    }
}

pub fn select(manifest: &Path, targets: &[String]) -> Result<SelectedProject, PreviewError> {
    crate::project::select_project(manifest, targets, &TargetOverrideArgs::default(), true)
        .map_err(|error| PreviewError::Project(error.to_string()))
}

pub fn the_window(selected: &SelectedProject) -> Result<(Installer, TargetTriple), PreviewError> {
    let config = selected
        .selected_targets
        .first()
        .ok_or_else(|| PreviewError::Project("this manifest declares no target".to_owned()))?;
    let installer = zup_manifest::compile(
        &selected.manifest,
        config,
        selected.overrides.get(&config.profile),
    )
    .map_err(|error| PreviewError::Project(format!("{}: {error}", selected.manifest_name)))?;
    if installer.frontend != Frontend::Gui {
        return Err(PreviewError::NoWindow {
            reason: format!(
                "target `{}` uses the `{}` frontend; `--target` names one that presents a window",
                config.profile, installer.frontend
            ),
        });
    }
    Ok((installer, config.target.clone()))
}

impl Driver for Session {
    type Change = BTreeSet<PathBuf>;
    type Finished = ();

    fn runtime(&mut self) -> &mut Runtime {
        &mut self.runtime
    }

    /// the work somewhere the loop never calls will announce that it is
    fn tick(&mut self) {
        if self.queued {
            self.queued = false;
            self.refresh();
        }
    }

    fn changed(&mut self, paths: BTreeSet<PathBuf>) {
        if classify(
            &paths,
            &self.manifest,
            self.package.as_deref(),
            &self.assets,
        )
        .is_some()
        {
            println!("  changed  re-resolving the application");
            self.queued = true;
        }
    }

    fn finished(&mut self, _outcome: ()) {}
}
