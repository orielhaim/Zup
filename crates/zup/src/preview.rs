//! `zup preview`: the application's real installer window, over a simulated machine.
//!
//! This is for an application author. `zup preset dev` is for a preset author, and
//! the two differ in what they watch and in where the window comes from: a preset
//! author compiles Rust, and an application author resolves a package. Below that
//! they are the same thing, because both run the same [`zup_preview`] against the
//! same protocol with the same controls - and a preset author who developed
//! against a different state machine would meet this one instead.
//!
//! The window is not a drawing of the application's installer. It *is* the one a
//! build composes: the same `.zupui`, read by the same reader, selected by the
//! same resolver, with the settings the manifest's `[ui.settings]` say. A preview
//! that resolved the UI its own way would be a preview of something nobody is
//! going to install, and every question it answered about it would be about the
//! wrong program.
//!
//! What is simulated is the machine, not the window. Nothing here writes an
//! application file, touches PATH, a registry, a shortcut, a service or an
//! uninstall record, elevates, or acquires anything. A control causes the event
//! an engine would have caused and the reducer decides what it means, so there is
//! no path from a button somebody pressed to a mutation on their machine.

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

/// Why a preview could not open, or could not continue.
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

/// Open this application's installer window over a simulated machine.
#[derive(Debug, Args)]
pub struct PreviewCommand {
    /// The manifest to resolve.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// A target profile name or triple, repeatable. Empty selects the only one, and
    /// a project that declares several must say which it means.
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
}

/// Run `zup preview`.
pub fn run(args: PreviewCommand, toolchain: Option<PathBuf>) -> miette::Result<()> {
    // What the session prints is its product, and a redirected stdout is block
    // buffered by default, so a preview would say nothing at all while it works.
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

/// The manifest a person meant, found the way every other command finds one.
///
/// A relative name is resolved against the working directory, and a project is
/// otherwise discovered by walking up from it, because `zup preview` is a command
/// somebody runs from inside a project rather than one they point at a file. A
/// directory that is not in a project is a refusal that says so, and it is a
/// refusal rather than a fallback: silently opening a demonstration in a
/// directory that has no application in it is the one answer that cannot be
/// acted on.
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
        // The default name, relative to the working directory.
        return search_upwards(Path::new("."));
    }
    // Something was named, and it is not there. Saying which file and where it was
    // looked for is more use than saying the operating system's error.
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Err(miette::miette!(
        "`{}` is not a file and not a directory, and no project was found in `{}` or above it",
        named.display(),
        here.display()
    ))
}

/// The first `zup.toml` at `start` or above it.
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

/// What changed in an application project, and therefore what is worth doing
/// about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The manifest changed, so the application is a different one.
    Configuration,
    /// One of the files the settings named changed.
    Asset,
    /// The package the application selected changed.
    Package,
}

/// The one change among `paths` that means something.
///
/// An application project holds a great deal that is none of a preview's
/// business: payload, plugin components, build output. A change to any of it is
/// not a change to the window, and reacting to it would replace a working window
/// because somebody edited a file two directories away.
///
/// A change to the manifest subsumes the other two, because the manifest is what
/// names them, and re-resolving reads the package and the files again anyway.
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

/// The preview session.
///
/// Public because it is what the command runs and what the tests drive. A preview
/// is worth nothing that cannot be exercised without a terminal, and the whole
/// claim is that it is worth exercising.
pub struct Session {
    runtime: Runtime,
    resolver: Arc<ToolchainResolver>,
    manifest: PathBuf,
    targets: Vec<String>,
    project_root: PathBuf,
    state: StateDirectory,
    /// The digest of the executable currently being presented.
    ///
    /// The address a window is compared by, for the same reason an installation
    /// addresses its own by digest: a package rebuilt in place is a different
    /// window, and a package that did not change is the same one however many
    /// times it has been re-read.
    presented: Option<Sha256Digest>,
    /// The package the last accepted resolution selected.
    package: Option<PathBuf>,
    /// The files it named, so an edit to one is a data change rather than an
    /// unrelated save.
    assets: Vec<PathBuf>,
    /// Whether a resolution is wanted and has not been attempted.
    queued: bool,
}

impl Session {
    /// Read the project, resolve its window, and open a session on the machine
    /// that application describes.
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

    /// Watch the project on a thread of its own.
    ///
    /// The thread reports the paths and nothing else. Classifying them needs to
    /// know what the session is *currently* showing, and that changes as the
    /// project is edited, so the judgement belongs on this side where the answers
    /// live rather than in a copy of them taken when the thread started.
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
                        // Said rather than swallowed. A session that has stopped
                        // watching must not look like one in which nothing
                        // changed.
                        Seen::Failed(reason) => Event::Report(format!("watching: {reason}")),
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
            })
            .expect("a watch thread");
    }

    /// Resolve the application again, from the manifest as it is now.
    ///
    /// Every step of the answer comes from the same resolver a build uses, so a
    /// preview and the installer it is a preview of cannot describe different
    /// applications.
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
            &zup_windows::WindowsSourceFilePolicy,
        )?)
    }

    /// Take a resolution: the files, the settings, and - only when the window
    /// itself changed - the child.
    ///
    /// The order is not incidental. Files and settings go first, so a child that
    /// opens after this already has them; and the replacement is last, so a
    /// failure anywhere above leaves the working window exactly where it was.
    pub fn adopt(&mut self, resolved: &Resolved) -> Result<(), PreviewError> {
        self.runtime.clear_assets();
        let mut files = Vec::with_capacity(resolved.assets.len());
        for asset in &resolved.assets {
            let Some(source) = asset.source.as_deref() else {
                continue;
            };
            let name = asset.name.to_string();
            // The bytes on disk are hashed as they are written, and the digest is
            // the one the resolution proved them at. A file that changed between
            // the two is a file nobody checked, so it is refused rather than shown
            // under a name that answer did not carry.
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
            // The same window. What the application configured changed, which is
            // the thing the session exists to show, and the child already showing
            // is still the one showing it.
            self.runtime.publish();
            return Ok(());
        }
        self.runtime
            .present(&resolved.runtime, &resolved.executable)
            .map_err(|error| PreviewError::Start(error.to_string()))?;
        self.presented = Some(digest);
        Ok(())
    }

    /// Resolve and adopt, keeping the current window if any step of that fails.
    pub fn refresh(&mut self) {
        let selected = match select(&self.manifest, &self.targets) {
            Ok(selected) => selected,
            // A manifest that no longer parses is the most ordinary thing a
            // person does while editing one, and it must not cost the window that
            // is already open.
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

    /// Run one turn of the session's own work, through the same code the loop runs.
    ///
    /// A child that has exited is noticed, what a child has asked for is validated
    /// and published, and a resolution that was queued is carried out. A caller
    /// driving the session by hand needs all three, and giving it a second copy of
    /// the loop is how a driver and a session would come to disagree about when a
    /// save was honoured.
    pub fn pump(&mut self) {
        Driver::pump(self);
        self.tick();
    }

    /// Whether the window has been closed, and the preview with it.
    pub fn closed(&self) -> bool {
        self.runtime.closed()
    }

    /// The state the presented window is drawing.
    pub fn state(&self) -> &zup_preset_protocol::Snapshot {
        self.runtime.snapshot()
    }

    /// What the presented window is told the application configured.
    pub fn configuration(&self) -> &zup_preset_protocol::Configuration {
        self.runtime.configuration()
    }

    /// The digest of the window on screen, and `None` before the first.
    ///
    /// The window's address, so a reader of the session's own output - or a test -
    /// can tell a replacement from a re-send of the same window.
    pub fn presented(&self) -> Option<Sha256Digest> {
        self.presented
    }

    /// The package the last accepted resolution selected.
    pub fn package(&self) -> Option<&Path> {
        self.package.as_deref()
    }

    /// The application-provided files the last accepted resolution named.
    pub fn assets(&self) -> &[PathBuf] {
        &self.assets
    }

    /// What a change on disk means, or `None` when it means nothing.
    ///
    /// Public because it is the judgement that decides whether a preview spends
    /// anything on a save, and a judgement worth stating is one worth testing.
    pub fn meaning(&self, paths: &BTreeSet<PathBuf>) -> Option<&'static str> {
        classify(paths, &self.manifest, self.package.as_deref(), &self.assets).map(|change| {
            match change {
                Change::Configuration => "the application",
                Change::Asset => "an asset",
                Change::Package => "the package",
            }
        })
    }

    /// One line of the control surface, and what it did to the simulated machine.
    ///
    /// The same controls `zup preset dev` offers, on the same session, because there
    /// is one machine being simulated and one set of things a person can do to it.
    pub fn control(&mut self, line: &str) -> ControlOutcome {
        self.runtime.control(line)
    }

    /// Something on disk changed, in the loop's own vocabulary.
    ///
    /// Public because it is what the session does with a watcher's paths, and a
    /// test that drove the session any other way would not be testing the loop.
    pub fn saw_change(&mut self, paths: BTreeSet<PathBuf>) {
        Driver::changed(self, paths);
    }

    /// End the preview, and take the child with it.
    pub fn end(&mut self) {
        self.runtime.shutdown();
    }
}

/// Read one target's project, or say why not.
pub fn select(manifest: &Path, targets: &[String]) -> Result<SelectedProject, PreviewError> {
    crate::project::select_project(manifest, targets, &TargetOverrideArgs::default(), true)
        .map_err(|error| PreviewError::Project(error.to_string()))
}

/// The compiled installer and target a preview presents, or why there is none.
///
/// A console or headless target genuinely presents no window, and that is not a
/// preview of anything: the honest answer is to say so rather than open a
/// presenter for a frontend the application does not have.
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

    /// Carry out a resolution a save asked for.
    ///
    /// Here rather than in `changed`, and this is the whole reason the method is
    /// not private: a driver that sets a flag in response to an event and puts
    /// the work somewhere the loop never calls will announce that it is
    /// re-resolving and then not do it. `tick` is what the loop calls every turn,
    /// so the work is here and a save cannot be lost.
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
