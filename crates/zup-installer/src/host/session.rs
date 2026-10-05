//! The session: a live preset, a live engine, and the loop that joins them.
//!
//! [`HostState`](super::HostState) is a pure state machine and this is
//! everything around it. It resolves the preset, launches it, exchanges
//! frames, carries out the decisions the state machine returns, and republishes
//! the whole snapshot after every change. Nothing a preset sends is acted on
//! without passing through `HostState::accept` first.
//!
//! The engine runs on its own thread per operation and reports through a
//! broadcast channel, because a transaction can be minutes long and this loop
//! has to stay responsive enough to answer a cancel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use zup_core::{Frontend, Installer, SelectedScope};
use zup_exec::{InstallLedger, LifecycleAction};
use zup_preset_protocol::{Action, UpdateState};
use zup_runtime::{CancellationHandle, RuntimeEvent};

use super::preset::{self, PresetError};
use super::{HostDecision, HostState, Selection};
use crate::lifecycle::{self, Request};
use crate::package;
use crate::state;

/// What the caller asked the window to open.
///
/// Apps & Features invokes the maintenance copy with `--scope` and
/// `--state-root`; a person double-clicks the installer with neither. Honouring
/// the two is not a convenience - a machine-scope modify that ignored the scope
/// it was registered with would open the window on the wrong installation.
pub struct Launch {
    /// Show the maintenance surface even if the path does not say so.
    pub maintenance: bool,
    /// Start the uninstall confirmation immediately.
    pub auto_uninstall: bool,
    /// The scope the caller named, when it named one.
    pub scope: Option<SelectedScope>,
    /// The state root the caller named, when it named one.
    pub state_root: Option<PathBuf>,
    /// The components the caller preselected, when it named any.
    pub preselected: Option<Vec<zup_core::ComponentId>>,
}

impl Launch {
    /// A double-click: no arguments, and whatever the path says about the role.
    pub fn from_path(executable: &Path) -> Self {
        Self {
            maintenance: zup_windows::is_maintenance_path(executable),
            auto_uninstall: false,
            scope: None,
            state_root: None,
            preselected: None,
        }
    }

    /// A launch that named a lifecycle, optionally asking for the window.
    pub fn from_arguments(args: &crate::cli::LifecycleArgs) -> Self {
        Self {
            maintenance: false,
            auto_uninstall: false,
            scope: Some(SelectedScope::from(args.scope)),
            state_root: args.state_root.clone(),
            preselected: (!args.enable.is_empty()).then(|| {
                args.enable
                    .iter()
                    .filter_map(|raw| zup_core::ComponentId::new(raw).ok())
                    .collect()
            }),
        }
    }

    /// The uninstall confirmation window, for Apps & Features on a GUI frontend.
    pub fn uninstall_confirmation(args: &crate::uninstall::UninstallArgs) -> Self {
        Self {
            maintenance: true,
            auto_uninstall: true,
            scope: args.scope.map(SelectedScope::from),
            state_root: args.state_root.clone(),
            preselected: None,
        }
    }
}

/// Where this session installs, and where it keeps its own record.
pub struct Placement {
    pub scope: SelectedScope,
    pub state_root: PathBuf,
    pub app_id: zup_core::AppId,
    /// The version already committed, which decides whether an install repairs,
    /// updates, or installs.
    pub installed_version: Option<semver::Version>,
}

/// Everything one graphical session is made of, before the pipe is opened.
///
/// Split from the loop so the two halves are testable apart: this half is a
/// pure function of what is on disk, and it is the only place that decides
/// which surface a person is looking at and which bytes that surface presents.
pub struct Opening {
    pub state: HostState,
    pub installer: Installer,
    /// The package the installer was composed with, when it carries one.
    ///
    /// Held rather than reopened because the preset's assets are read out of it,
    /// and reading them through the same verified bundle the plan came from is
    /// what makes "the application configured a logo" mean "these are the bytes of
    /// that logo".
    pub bundle: Option<zup_windows::EmbeddedBundle>,
    /// Where this window's preset comes from.
    ///
    /// An installation reads what it owns; an install that has committed nothing
    /// reads the image it was launched from. There is no third option and no
    /// fallback between them: an installation whose recorded preset content is gone
    /// has a problem to report, not a window to open some other way.
    pub ui: PresetSource,
    pub placement: Placement,
    pub launch: Launch,
}

/// The window this launch will present, and where its bytes are.
pub enum PresetSource {
    /// Not installed yet: this image's own preset, and nothing committed.
    Composed { preset: zup_core::PresetRuntime },
    /// Installed: the window the ledger recorded, under the runtime's directory.
    Installed {
        runtime: zup_core::InstalledPreset,
        directory: PathBuf,
        /// The target this installation installed for, which decides the name its
        /// content was persisted under.
        target: zup_core::TargetTriple,
    },
}

impl PresetSource {
    fn as_source<'a>(
        &'a self,
        executable: &'a Path,
        bundle: &'a zup_windows::EmbeddedBundle,
    ) -> preset::Source<'a> {
        match self {
            Self::Composed { preset } => preset::Source::Composed {
                executable,
                preset,
                bundle,
            },
            Self::Installed {
                runtime,
                directory,
                target,
            } => preset::Source::Installed {
                directory,
                runtime,
                target,
            },
        }
    }
}

/// Read a package and decide what its window opens on.
///
/// An installation that already exists gets the maintenance surface, because
/// that is the surface whose actions are true of the machine. A package that is
/// not installed gets the choices. A caller that asked for maintenance over
/// nothing installed is refused rather than shown an empty window.
pub fn opening(executable: &Path, launch: Launch) -> miette::Result<Opening> {
    let bundle = package::open_bundle_if_present(executable)?.ok_or_else(|| {
        miette::miette!("this file carries no installer package; run the installer you downloaded")
    })?;
    let installer = package::target_plan(&bundle)?.installer;
    let bundle = Some(bundle);
    let installed = find_installation(&installer, launch.state_root.as_deref())?;
    if launch.maintenance && installed.is_none() {
        return Err(miette::miette!("the installed application was not found"));
    }

    let launchers = zup_preset_host::surface::launchers(&installer);
    let state = match &installed {
        Some((scope, ledger)) => {
            let maintenance =
                zup_preset_host::surface::maintenance_state(&installer, ledger, *scope);
            HostState::maintenance(
                zup_preset_host::surface::product(&installer),
                maintenance,
                zup_preset_host::surface::capabilities(&installer, true),
            )
        }
        None => {
            let scope = launch
                .scope
                .unwrap_or_else(|| zup_preset_host::surface::default_scope(&installer));
            let options = zup_preset_host::surface::install_options(
                &installer,
                scope,
                None,
                launch.preselected.as_deref(),
                None,
            );
            HostState::install(
                zup_preset_host::surface::product(&installer),
                options,
                zup_preset_host::surface::capabilities(&installer, false),
            )
        }
    }
    .with_launchers(launchers);

    let scope = installed
        .as_ref()
        .map(|(scope, _)| *scope)
        .unwrap_or_else(|| {
            zup_preset_host::convert::engine_scope(state.snapshot().surface.scope())
        });
    let state_root = state::resolve_state_root(launch.state_root.clone(), scope)?;
    let placement = Placement {
        scope,
        state_root: state_root.clone(),
        app_id: installer.app.id.clone(),
        installed_version: installed.as_ref().map(|(_, ledger)| ledger.version.clone()),
    };
    let ui = match &installed {
        Some((_, ledger)) => {
            let runtime = ledger.preset().cloned().ok_or_else(|| {
                miette::miette!(
                    "{} is installed with no recorded window, and its maintenance runtime cannot \
                     present one.\n\nReinstall it to restore the window it was installed with.",
                    installer.app.name
                )
            })?;
            let directory = zup_windows::maintenance_directory(
                &state_root,
                &installer.app.id,
                scope,
                &ledger.version,
            );
            PresetSource::Installed {
                runtime,
                directory,
                target: installer.target.clone(),
            }
        }
        None => PresetSource::Composed {
            preset: installer
                .preset
                .clone()
                .ok_or_else(|| miette::miette!("this installer was composed without a preset"))?,
        },
    };
    Ok(Opening {
        state,
        installer,
        bundle,
        ui,
        placement,
        launch,
    })
}

/// The one installation this application has.
fn find_installation(
    installer: &Installer,
    state_root: Option<&Path>,
) -> miette::Result<Option<(SelectedScope, InstallLedger)>> {
    for scope in zup_preset_host::surface::scopes(installer)
        .into_iter()
        .map(zup_preset_host::convert::engine_scope)
    {
        let root = state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?;
        let ledger = zup_windows::InstallLedgerStore::new(root)
            .load(&installer.app.id, scope)
            .ok()
            .flatten();
        if let Some(ledger) = ledger {
            return Ok(Some((scope, ledger)));
        }
    }
    Ok(None)
}

/// What an engine thread reports back to the loop.
///
/// One channel rather than two because the loop republishes the whole snapshot
/// after every message anyway: a caller that had to reconcile progress, a plan,
/// and an update result against each other would be reconstructing state the
/// host already owns.
#[derive(Clone)]
enum Report {
    Engine(RuntimeEvent),
    /// The plan for one generation of choices. An older generation is stale.
    Plan(u64, Result<zup_presentation::PlanPreview, String>),
    Update(UpdateReport),
    RepairDrift(Vec<String>),
}

#[derive(Clone)]
enum UpdateReport {
    Status(UpdateState),
    Finished(Result<(), String>),
}

type Reports = tokio::sync::broadcast::Sender<Report>;
type Inbox = tokio::sync::broadcast::Receiver<Report>;

/// The running operation, shared with the loop so a cancel reaches the engine
/// thread without a preset in the path.
#[derive(Default)]
struct Active {
    cancel: Option<CancellationHandle>,
}

impl Active {
    fn set(&mut self, cancel: CancellationHandle) {
        self.cancel = Some(cancel);
    }

    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

/// Run one graphical session to its end.
///
/// The preset is disposable: if it exits, the loop returns and the installation
/// is exactly where it was. Failing to open a window at all has still not
/// touched the machine.
pub async fn run(executable: &Path, launch: Launch) -> miette::Result<()> {
    let Opening {
        mut state,
        installer,
        placement,
        launch,
        bundle,
        ui,
    } = opening(executable, launch)?;

    let bundle = bundle.ok_or_else(|| {
        miette::miette!("this file carries no installer package; run the installer you downloaded")
    })?;
    let composed = preset::materialize(&ui.as_source(executable, &bundle), state.capabilities())
        .map_err(preset_error)?;
    let configuration = composed.configuration.clone();

    let mut preset = preset::launch(
        &composed.executable,
        state.capabilities().clone(),
        state.snapshot().product.clone(),
    )
    .map_err(session_error)?;

    // One thread reads what the preset asks for, so the loop below is free to
    // wait on the engine at the same time. The reading is the only part of a
    // session that blocks, and it blocks on a child, not on the machine.
    let (asked, mut questions) = tokio::sync::mpsc::unbounded_channel();
    {
        let reader = preset.take_reader();
        std::thread::Builder::new()
            .name("zup-preset-actions".into())
            .spawn(move || {
                while let Some(action) = reader.next() {
                    if asked.send(action).is_err() {
                        return;
                    }
                }
            })
            .map_err(|error| miette::miette!("start the preset reader: {error}"))?;
    }

    let active = Arc::new(Mutex::new(Active::default()));
    let (reports, mut inbox) = tokio::sync::broadcast::channel::<Report>(256);
    let mut plans = Plans::default();

    if launch.auto_uninstall {
        state.accept(Action::RequestUninstall);
    }
    if let Some(selection) = state.plan_request() {
        plans.request(selection, &reports);
    }
    // Whether the preset can still be written to. It starts true and goes false the
    // first time a write finds the transport gone, which is a preset that has closed
    // its window and left. The questions it already asked are still answered - it
    // asked before it went, and the host owns the installation either way - so the
    // loop runs out of questions rather than out of window.
    let mut deliverable = publish(&mut preset, &state, &configuration)?;

    loop {
        if !preset.is_running() {
            break;
        }
        tokio::select! {
            biased;
            report = inbox.recv() => {
                match report.map_err(lagged)? {
                    Report::Engine(event) => state.observe(&event),
                    Report::Plan(generation, plan) => {
                        if generation != plans.generation {
                            continue;
                        }
                        match plan {
                            Ok(plan) => state.set_plan(plan),
                            Err(message) => state.plan_failed(message),
                        }
                    }
                    Report::Update(UpdateReport::Status(status)) => state.set_update(None, status),
                    Report::Update(UpdateReport::Finished(Ok(()))) => {}
                    Report::Update(UpdateReport::Finished(Err(message))) => {
                        state.fail(message, false);
                    }
                    Report::RepairDrift(resources) => state.set_repair_drift(resources),
                }
                if deliverable {
                    deliverable = publish(&mut preset, &state, &configuration)?;
                }
            }
            asked = questions.recv() => {
                let Some(action) = asked else { break };
                if action == Action::Close {
                    break;
                }
                act(
                    &mut state,
                    &installer,
                    &placement,
                    action,
                    &active,
                    &reports,
                    &mut inbox,
                    &mut plans,
                );
                if deliverable {
                    deliverable = publish(&mut preset, &state, &configuration)?;
                }
            }
        }
    }

    active.lock().expect("the running operation").cancel();
    // The session is over, and the child this host launched is still running.
    // Dropping the owner ends the preset and everything it started, which is what
    // releases the executable it was launched from: an uninstall that follows
    // would otherwise fail on a file a window the user already closed is still
    // holding open.
    drop(preset);
    Ok(())
}

fn lagged(error: tokio::sync::broadcast::error::RecvError) -> miette::Report {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(skipped) => {
            miette::miette!("the engine reported {skipped} events past the window")
        }
        tokio::sync::broadcast::error::RecvError::Closed => miette::miette!("the engine ended"),
    }
}

/// Tell the preset the whole state, in full, and say whether it heard.
///
/// Every message, not every change: a snapshot is complete, so republishing it
/// costs one frame and removes every question about whether the preset missed
/// something. The configuration rides with the first one only - the settings and
/// asset table are what the application configured, and they do not change while
/// one session runs - so a republish after a click carries the state alone.
///
/// `false` means the preset is no longer on the other end and the caller should
/// stop trying. That is one failure out of the several a publish can have, and it
/// is the only one that is not a fault: a preset sends its last question and goes.
/// It closed its window, which is what a person closing a window looks like from
/// here, and it has by definition not waited to hear the answer. Nothing after it
/// can be delivered, so the fact is about delivery rather than about the
/// installation, and the host owns the installation whether or not anybody is
/// watching it happen.
///
/// Everything else still fails the run. A protocol or configuration error, or a
/// transport that broke for some reason other than the peer having gone, is a real
/// fault and is reported as one: `is_ok()` here once swallowed those too, so a
/// broken publish and a closed window were indistinguishable to the caller and
/// both were read as "the window is gone".
fn publish(
    preset: &mut preset::PresetProcess,
    state: &HostState,
    configuration: &zup_preset_protocol::Configuration,
) -> miette::Result<bool> {
    match preset.publish(configuration.clone(), Box::new(state.snapshot().clone())) {
        Ok(()) => Ok(true),
        Err(preset::SessionError::Disconnected(_)) => Ok(false),
        Err(error) => Err(miette::miette!("{error}")),
    }
}

/// The plans this session has asked for.
///
/// Every change of choice starts a new one, and only the newest answer is
/// kept: a person who toggles three components quickly gets the plan for the
/// third, whatever order the three finish in.
#[derive(Default)]
struct Plans {
    generation: u64,
}

impl Plans {
    fn request(&mut self, selection: Selection, reports: &Reports) {
        self.generation += 1;
        let generation = self.generation;
        let reports = reports.clone();
        let executable = package::current_executable().map_err(|error| error.to_string());
        spawn("setup-plan", move || {
            let plan = executable.and_then(|executable| preview(&executable, selection));
            let _ = reports.send(Report::Plan(generation, plan));
        });
    }
}

/// Carry out one decision the state machine reached.
#[allow(clippy::too_many_arguments)]
fn act(
    state: &mut HostState,
    installer: &Installer,
    placement: &Placement,
    action: Action,
    active: &Arc<Mutex<Active>>,
    reports: &Reports,
    inbox: &mut Inbox,
    plans: &mut Plans,
) {
    match state.accept(action) {
        HostDecision::Run {
            action,
            selection,
            cleanup_lock,
        } => {
            *inbox = reports.subscribe();
            let cancel = CancellationHandle::new();
            active
                .lock()
                .expect("the running operation")
                .set(cancel.clone());
            let operation = Operation {
                action,
                selection,
                cleanup_lock,
            };
            let installer = installer.clone();
            let placement = Placement {
                scope: placement.scope,
                state_root: placement.state_root.clone(),
                app_id: placement.app_id.clone(),
                installed_version: placement.installed_version.clone(),
            };
            let reports = reports.clone();
            spawn("setup-lifecycle", move || {
                run_lifecycle(operation, installer, placement, cancel, reports);
            });
        }
        HostDecision::Plan(selection) => plans.request(selection, reports),
        HostDecision::Update => {
            let executable = match package::current_executable() {
                Ok(executable) => executable,
                Err(error) => return state.fail(error.to_string(), false),
            };
            let scope = placement.scope;
            let sender = reports.clone();
            spawn("setup-update", move || {
                let mut status = |progress| {
                    let _ = sender.send(Report::Update(UpdateReport::Status(progress)));
                };
                let result =
                    crate::update::from_maintenance_surface(&executable, scope, &mut status);
                let _ = sender.send(Report::Update(UpdateReport::Finished(result)));
            });
        }
        HostDecision::Launch(target) => {
            let selection = Selection::from_surface(&state.snapshot().surface);
            spawn("setup-launch", move || {
                let started = package::current_executable()
                    .map_err(|error| error.to_string())
                    .and_then(|executable| launch(&executable, selection, &target.name));
                if let Err(error) = started {
                    eprintln!("{} did not start: {error}", target.name);
                }
            });
        }
        HostDecision::Cancel => active.lock().expect("the running operation").cancel(),
        HostDecision::OpenLog => {
            if let Some(path) = state.log_path()
                && let Err(error) = opener::open(path)
            {
                state.fail(format!("open the session log: {error}"), false);
            }
        }
        HostDecision::CopyDiagnostics => {
            copy_to_clipboard(&diagnostic_summary(state.log_path()));
        }
        HostDecision::Acknowledged | HostDecision::Refused(_) => {}
    }
}

/// Start a thread that owns no UI state and reports everything it does.
fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(body);
}

/// What a decision to run carries into the engine thread.
struct Operation {
    action: LifecycleAction,
    selection: Selection,
    cleanup_lock: bool,
}

impl Operation {
    /// The components this operation turns off: everything optional that was
    /// not chosen.
    fn disabled(&self, installer: &Installer) -> Vec<String> {
        installer
            .components
            .iter()
            .filter(|component| {
                !component.required && !self.selection.components.contains(&component.id)
            })
            .map(|component| component.id.to_string())
            .collect()
    }

    fn enabled(&self) -> Vec<String> {
        self.selection
            .components
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn directory(&self) -> Option<PathBuf> {
        self.selection
            .install_directory
            .as_deref()
            .map(PathBuf::from)
    }
}

/// Prepare and run one lifecycle, reporting every event on the channel.
fn run_lifecycle(
    operation: Operation,
    installer: Installer,
    placement: Placement,
    cancel: CancellationHandle,
    reports: Reports,
) {
    let prepared = prepare(
        &operation,
        &installer,
        placement.scope,
        Some(placement.state_root.clone()),
        &lifecycle::RuntimeCancellationQuery(&cancel),
        true,
    );
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = reports.send(Report::Engine(RuntimeEvent::Failed {
                kind: "prepare_failed".into(),
                message: error.to_string(),
            }));
            return;
        }
    };

    let drifted = prepared
        .request
        .transaction_plan
        .retired_keys
        .iter()
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>();

    let (engine, _) = tokio::sync::broadcast::channel(256);
    let mut received = engine.subscribe();
    let pump = {
        let reports = reports.clone();
        std::thread::spawn(move || {
            while let Ok(event) = received.blocking_recv() {
                let terminal = crate::execute::is_terminal(&event);
                if reports.send(Report::Engine(event)).is_err() || terminal {
                    break;
                }
            }
        })
    };

    let outcome = crate::execute::execute_with_control(prepared, cancel, engine);
    let _ = pump.join();

    if matches!(operation.action, LifecycleAction::Repair { .. })
        && matches!(outcome, Ok(zup_runtime::InstallOutcome::Committed))
        && !drifted.is_empty()
    {
        let _ = reports.send(Report::RepairDrift(drifted));
    }
    if operation.cleanup_lock
        && let Ok(zup_runtime::InstallOutcome::Committed) = outcome
    {
        let _ = crate::uninstall::remove_lock(
            &placement.state_root,
            &placement.app_id,
            placement.scope,
        );
        let _ = zup_windows::cleanup_app_payload_overlays(
            &placement.state_root,
            &placement.app_id,
            placement.scope,
        );
    }
}

/// Prepare a lifecycle, for the two callers that share every argument but the
/// cancellation query and whether prerequisites are acquired.
fn prepare(
    operation: &Operation,
    installer: &Installer,
    scope: SelectedScope,
    state_root: Option<PathBuf>,
    cancellation: &dyn zup_plan::CancellationQuery,
    acquire_prerequisites: bool,
) -> miette::Result<lifecycle::PreparedRuntime> {
    lifecycle::prepare_embedded_transition_with_cancellation(
        Request::Named(operation.action),
        scope,
        state_root,
        operation.enabled(),
        operation.disabled(installer),
        operation.directory(),
        lifecycle::EmbeddedPreparationMode {
            cancellation,
            acquire_prerequisites,
        },
    )
}

/// What this machine would do, so the window can show a cost before a click.
///
/// Planned rather than prepared: a preview answers "what would this do", and a
/// preview that acquired a prerequisite would be a download nobody asked for.
fn preview(
    executable: &Path,
    selection: Selection,
) -> Result<zup_presentation::PlanPreview, String> {
    let (plan, installer) = plan(executable, &selection)?;
    let mut preview = zup_presentation::PlanPreview::from_install_plan(&plan)
        .with_declared_prerequisites(&installer.prerequisites);
    if let Ok(target) = zup_windows::resolve_target(
        &plan,
        &zup_windows::WindowsTargetContext::new(selection.scope),
    ) {
        preview.install_directory = target.install_directory.to_string();
        preview.estimated_bytes = target.summary.install_bytes;
        preview.requires_authorization = target.summary.requires_authorization;
    }
    Ok(preview)
}

/// Start the installed application through the launcher named `name`.
///
/// Resolved from the same plan the installation committed, so the program that
/// starts is the one the launcher points at and never a path a preset supplied.
fn launch(executable: &Path, selection: Selection, name: &str) -> Result<(), String> {
    let (plan, _) = plan(executable, &selection)?;
    let target = zup_windows::resolve_target(
        &plan,
        &zup_windows::WindowsTargetContext::new(selection.scope),
    )
    .map_err(found)?;
    let launcher = target
        .launchers
        .iter()
        .find(|launcher| launcher.name.as_str() == name)
        .ok_or_else(|| format!("no launcher is named `{name}`"))?;
    let mut command = std::process::Command::new(launcher.target.as_str());
    command.args(&launcher.arguments);
    if let Some(directory) = &launcher.working_directory {
        command.current_dir(directory.as_str());
    }
    command.spawn().map(drop).map_err(found)
}

/// The plan this package would follow for `selection`.
fn plan(
    executable: &Path,
    selection: &Selection,
) -> Result<(zup_plan::InstallPlan, Installer), String> {
    let build =
        package::target_plan(&package::open_bundle(executable).map_err(found)?).map_err(found)?;
    let installer = build.installer.clone();
    let mut request = zup_plan::PlanRequest::new(installer.target.clone(), selection.scope);
    for component in &installer.components {
        if selection.components.contains(&component.id) {
            request.components.enable.insert(component.id.clone());
        } else if !component.required {
            request.components.disable.insert(component.id.clone());
        }
    }
    request.install_directory = selection
        .install_directory
        .as_deref()
        .map(PathBuf::from)
        .as_deref()
        .map(state::install_directory_template)
        .transpose()
        .map_err(found)?;

    let plan = zup_plan::plan(
        &zup_core::BuildPlan {
            targets: vec![build],
        },
        &request,
    )
    .map_err(found)?;
    Ok((plan, installer))
}

fn found(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// The window, for a launch that named no operation.
pub fn surface(executable: &Path) -> miette::Result<()> {
    block_on(run(executable, Launch::from_path(executable)))
}

/// The window, for a launch that named a lifecycle with `--ui`.
pub fn surface_from_arguments(
    context: crate::context::RuntimeContext,
    args: crate::cli::LifecycleArgs,
) -> miette::Result<()> {
    if context.frontend != Frontend::Gui {
        return Err(miette::miette!("this build has no graphical surface"));
    }
    let executable = package::current_executable()?;
    block_on(run(&executable, Launch::from_arguments(&args)))
}

/// The window, from Apps & Features, where the file it is running from is the
/// maintenance copy Windows is about to delete.
pub fn uninstall_confirmation(
    executable: &Path,
    args: &crate::uninstall::UninstallArgs,
) -> miette::Result<()> {
    block_on(run(executable, Launch::uninstall_confirmation(args)))
}

/// Run a session on an executor of its own.
///
/// The setup binary's entry points are synchronous and this is the only place
/// that needs one, so the executor is built here rather than threaded through
/// the CLI.
fn block_on(future: impl std::future::Future<Output = miette::Result<()>>) -> miette::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("start the session runtime: {error}"))?
        .block_on(future)
}

/// What "Copy diagnostics" puts on the clipboard.
///
/// The session log, and a statement of what is *not* in it. A user pasting a
/// diagnostic into a bug report should be able to say, without reading the code,
/// that no environment variables and no secrets are in the text they are
/// sending.
fn diagnostic_summary(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "zup setup diagnostic\nlog: {path}\nNo environment variables or secrets are included."
        ),
        None => "zup setup diagnostic\nNo session log is available yet.".into(),
    }
}

fn copy_to_clipboard(value: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("clip").arg(value).output();
    }
    #[cfg(not(windows))]
    {
        let _ = value;
    }
}

fn preset_error(error: PresetError) -> miette::Report {
    miette::miette!("{error}")
}

fn session_error(error: zup_preset_host::SessionError) -> miette::Report {
    miette::miette!("{error}")
}
