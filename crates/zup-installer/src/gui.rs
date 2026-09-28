//! The graphical frontend: a window, a progress bar, and a cancel button.
//!
//! The window is a view. Every decision it can produce - install, modify, repair,
//! uninstall, update, cancel - is turned into the same engine request a command
//! line would produce, on a thread that owns no UI state, so a window and a
//! script install the same bytes through the same transaction.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use zup_core::{ComponentId, Frontend, InstallScope, SelectedScope};
use zup_exec::LifecycleAction;
use zup_presentation::{InstallationHealth, PlanPreview, UpdatePresentation};
use zup_runtime::{CancellationHandle, InstallOutcome, RuntimeEvent};
use zup_windows::{EmbeddedBundle, InstallLedgerStore};

use crate::cli::LifecycleArgs;
use crate::context::RuntimeContext;
use crate::execute;
use crate::lifecycle::{self, PreparedRuntime, Request};
use crate::package;
use crate::state;
use crate::uninstall::UninstallArgs;
use crate::update;

/// What the launcher asked the window to open.
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
    pub preselected: Option<Vec<ComponentId>>,
}

impl Launch {
    /// A double-click: no arguments, and whatever the path says about the role.
    pub fn from_path(executable: &Path) -> Self {
        Self {
            maintenance: zup_windows::is_maintenance_executable(executable),
            auto_uninstall: false,
            scope: None,
            state_root: None,
            preselected: None,
        }
    }

    /// A launch that named a lifecycle, optionally asking for the window.
    pub fn from_arguments(args: &LifecycleArgs) -> Self {
        Self {
            maintenance: false,
            auto_uninstall: false,
            scope: Some(SelectedScope::from(args.scope)),
            state_root: args.state_root.clone(),
            preselected: (!args.enable.is_empty()).then(|| {
                args.enable
                    .iter()
                    .filter_map(|raw| ComponentId::new(raw).ok())
                    .collect()
            }),
        }
    }

    /// The uninstall confirmation window, for Apps & Features on a GUI package.
    pub fn uninstall_confirmation(args: &UninstallArgs) -> Self {
        Self {
            maintenance: true,
            auto_uninstall: true,
            scope: args.scope.map(SelectedScope::from),
            state_root: args.state_root.clone(),
            preselected: None,
        }
    }
}

/// The window, for a launch.
pub fn install_surface(
    executable: &Path,
    bundle: &EmbeddedBundle,
    launch: Launch,
) -> miette::Result<()> {
    let build = package::target_plan(bundle)?;
    let installer = &build.installer;
    let installed = find_installation(installer, launch.state_root.as_deref())?;
    let selected_scope = installed.as_ref().map_or_else(
        || launch.scope.unwrap_or_else(|| first_scope(installer)),
        |(scope, _)| *scope,
    );
    let components = installer
        .components
        .iter()
        .map(|component| zup_ui::ComponentOption {
            id: component.id.clone(),
            name: component.name.to_string(),
            description: component.description.clone(),
            required: component.required,
            selected: launch.preselected.as_ref().map_or_else(
                || {
                    installed
                        .as_ref()
                        .map_or(component.default || component.required, |(_, ledger)| {
                            ledger.selected_components.contains(&component.id)
                        })
                },
                |preselected| preselected.contains(&component.id),
            ),
        })
        .collect::<Vec<_>>();
    let identity = zup_ui::ProductIdentity {
        name: installer.app.name.to_string(),
        publisher: installer
            .app
            .publisher
            .as_ref()
            .map(|publisher| publisher.to_string()),
        version: installer.app.version.to_string(),
        description: installer.app.description.clone(),
    };
    let preview = plan_preview(&build, selected_scope, installed.as_ref().map(|(_, l)| l));
    let surface = if launch.maintenance {
        let (_, ledger) = installed
            .as_ref()
            .ok_or_else(|| miette::miette!("the installed application was not found"))?;
        zup_ui::Surface::Maintenance {
            identity,
            installed_version: ledger.version.to_string(),
            components,
            updates_enabled: installer.updates.is_some(),
            scope: selected_scope,
            install_directory: state::persisted_install_directory(Some(ledger))
                .map(|template| template.to_string()),
            health: InstallationHealth {
                state: "Ready".into(),
                summary: "Up to date".into(),
                drift_count: 0,
            },
        }
    } else {
        zup_ui::Surface::Installer {
            identity,
            install: zup_ui::InstallModel {
                existing_version: installed
                    .as_ref()
                    .map(|(_, ledger)| ledger.version.to_string()),
                scopes: if installed.is_some() {
                    vec![selected_scope]
                } else {
                    all_scopes(installer)
                },
                selected_scope,
                components,
                install_directory: preview
                    .as_ref()
                    .map(|preview| preview.install_directory.clone()),
                allow_directory_override: installer.install.allow_directory_override,
                estimated_bytes: preview
                    .as_ref()
                    .map_or(0, |preview| preview.estimated_bytes),
                requires_authorization: preview
                    .as_ref()
                    .is_some_and(|preview| preview.requires_authorization),
                preview: preview.clone(),
            },
        }
    };
    let (commands_tx, commands_rx) = std::sync::mpsc::channel();
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let state = installed
        .map(|(scope, ledger)| {
            state::resolve_state_root(launch.state_root.clone(), scope)
                .map(|root| (scope, root, ledger.version))
        })
        .transpose()?;
    let package_version = installer.app.version.clone();
    let executable = executable.to_path_buf();
    std::thread::Builder::new()
        .name("setup-runtime".into())
        .spawn(move || {
            bridge(
                executable,
                selected_scope,
                state,
                package_version,
                commands_rx,
                events_tx,
            )
        })
        .map_err(|error| miette::miette!("start the runtime bridge: {error}"))?;
    if launch.auto_uninstall {
        commands_tx
            .send(zup_ui::UiCommand::ConfirmUninstall)
            .map_err(|error| miette::miette!("start the uninstall session: {error}"))?;
    }
    zup_ui::run_with_branding(surface, commands_tx, events_rx, installer.ui.clone());
    Ok(())
}

/// The window, for a launch that named a lifecycle with `--ui`.
pub fn install_surface_from_arguments(
    context: RuntimeContext,
    args: LifecycleArgs,
) -> miette::Result<()> {
    if context.frontend != Frontend::Gui {
        return Err(miette::miette!("this build has no graphical surface"));
    }
    let executable = package::current_executable()?;
    let bundle = package::open_bundle_if_present(&executable)?.ok_or_else(|| {
        miette::miette!("this file carries no installer package; run the installer you downloaded")
    })?;
    install_surface(&executable, &bundle, Launch::from_arguments(&args))
}

/// The window, from Apps & Features, where the file it is running from is the
/// maintenance copy Windows is about to delete.
pub fn uninstall_confirmation(
    executable: &Path,
    bundle: &EmbeddedBundle,
    args: &UninstallArgs,
) -> miette::Result<()> {
    install_surface(executable, bundle, Launch::uninstall_confirmation(args))
}

fn all_scopes(installer: &zup_core::Installer) -> Vec<SelectedScope> {
    match installer.install.scope {
        InstallScope::User => vec![SelectedScope::User],
        InstallScope::Machine => vec![SelectedScope::Machine],
        InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    }
}

fn first_scope(installer: &zup_core::Installer) -> SelectedScope {
    state::default_install_scope(installer.install.scope)
}

/// The one installation this application has.
fn find_installation(
    installer: &zup_core::Installer,
    state_root: Option<&Path>,
) -> miette::Result<Option<(SelectedScope, zup_exec::InstallLedger)>> {
    for scope in all_scopes(installer) {
        let root = state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?;
        let ledger = InstallLedgerStore::new(root)
            .load(&installer.app.id, scope)
            .ok()
            .flatten();
        if let Some(ledger) = ledger {
            return Ok(Some((scope, ledger)));
        }
    }
    Ok(None)
}

/// What this machine would do, so the window can show a cost before a click.
fn plan_preview(
    build: &zup_plan::TargetBuildPlan,
    scope: SelectedScope,
    installed: Option<&zup_exec::InstallLedger>,
) -> Option<PlanPreview> {
    let installer = &build.installer;
    let mut request = zup_plan::PlanRequest::new(installer.target.clone(), scope);
    match installed {
        Some(ledger) => {
            for component in &installer.components {
                if ledger.selected_components.contains(&component.id) {
                    request.components.enable.insert(component.id.clone());
                } else if !component.required {
                    request.components.disable.insert(component.id.clone());
                }
            }
        }
        None => {
            for component in &installer.components {
                if component.default || component.required {
                    request.components.enable.insert(component.id.clone());
                } else {
                    request.components.disable.insert(component.id.clone());
                }
            }
        }
    }
    if let Some(path) = state::persisted_install_directory(installed) {
        request.install_directory = Some(path);
    }
    let planning = zup_plan::BuildPlan {
        targets: vec![build.clone()],
    };
    zup_plan::plan(&planning, &request).ok().map(|install| {
        let mut preview = PlanPreview::from_install_plan(&install);
        if let Ok(target) =
            zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
        {
            preview.install_directory = target.install_directory.to_string();
            preview.estimated_bytes = target.summary.install_bytes;
            preview.requires_authorization = target.summary.requires_authorization;
        }
        preview
    })
}

/// A retry the window offered, kept so the same intent can be re-run.
#[derive(Clone)]
struct RetryIntent {
    scope: SelectedScope,
    action: LifecycleAction,
    components: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    cleanup_lock: bool,
}

/// The slots the bridge shares with the thread that runs a lifecycle.
struct Bridge {
    cancel: Arc<Mutex<Option<CancellationHandle>>>,
    retry: Arc<Mutex<Option<RetryIntent>>>,
    log: Arc<Mutex<Option<String>>>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
}

/// Turn the window's commands into engine requests, and the engine's events back
/// into window events.
///
/// One thread owns this loop. The lifecycle itself runs on its own thread per
/// operation, because a transaction can be minutes long and a window has to stay
/// responsive enough to be cancelled.
fn bridge(
    executable: PathBuf,
    default_scope: SelectedScope,
    installed: Option<(SelectedScope, PathBuf, semver::Version)>,
    package_version: semver::Version,
    commands: std::sync::mpsc::Receiver<zup_ui::UiCommand>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
) {
    use zup_ui::{UiCommand as Command, UiEvent as Event};

    let bridge = Bridge {
        cancel: Arc::new(Mutex::new(None)),
        retry: Arc::new(Mutex::new(None)),
        log: Arc::new(Mutex::new(None)),
        events: events.clone(),
    };
    for command in commands {
        match command {
            Command::Cancel => match bridge.cancel.lock().expect("cancel state").as_ref() {
                Some(cancel) => {
                    cancel.cancel();
                    let _ = events.send(Event::CancellationWaiting);
                }
                None => {
                    let _ = events.send(Event::OperationFinished(InstallOutcome::Cancelled));
                }
            },
            Command::Uninstall => {
                let _ = events.send(Event::ConfirmUninstall);
            }
            Command::DismissUninstall => {
                let _ = events.send(Event::DismissUninstall);
            }
            Command::Close => {
                let _ = events.send(Event::Quit);
            }
            Command::OpenLog => match bridge.log.lock().expect("log state").clone() {
                Some(path) => open_log(&path),
                None => {
                    let _ = events.send(Event::LogPath(session_log_path().display().to_string()));
                }
            },
            Command::CopyDiagnostics => {
                let summary = diagnostic_summary(bridge.log.lock().expect("log state").as_deref());
                copy_to_clipboard(&summary);
            }
            Command::Update => {
                let executable = executable.clone();
                let events = events.clone();
                let (scope, current) = installed.as_ref().map_or(
                    (default_scope, "unknown".to_owned()),
                    |(scope, _, version)| (*scope, version.to_string()),
                );
                std::thread::spawn(move || {
                    let status = events.clone();
                    let result =
                        update::from_maintenance_surface(&executable, scope, move |text| {
                            let _ = status.send(Event::UpdateStatus(UpdatePresentation {
                                channel: None,
                                state: text.into(),
                                current: None,
                                available: None,
                            }));
                        });
                    match result {
                        Ok(Some((from, available))) => {
                            let _ = events.send(Event::UpdateAvailable {
                                current: from,
                                available,
                            });
                        }
                        Ok(None) => {
                            let _ = events.send(Event::UpToDate { current });
                        }
                        Err(message) => {
                            let _ = events.send(Event::Error {
                                message,
                                recovery_required: false,
                            });
                        }
                    }
                });
            }
            Command::Preview {
                scope,
                components,
                install_directory,
            } => {
                let events = events.clone();
                let executable = executable.clone();
                let installed_version = installed.as_ref().map(|(_, _, version)| version.clone());
                std::thread::spawn(move || {
                    if let Err(error) = preview(
                        &executable,
                        scope,
                        components,
                        install_directory.map(PathBuf::from),
                        installed_version,
                        &events,
                    ) {
                        let _ = events.send(Event::Error {
                            message: error.to_string(),
                            recovery_required: false,
                        });
                    }
                });
            }
            Command::Install {
                scope,
                components,
                install_directory,
            } => {
                let action = state::resolve_applied_action(
                    installed.as_ref().map(|(_, _, version)| version),
                    &package_version.to_string(),
                );
                match action {
                    Ok(action) => start(
                        &executable,
                        scope,
                        action,
                        components,
                        install_directory.map(PathBuf::from),
                        false,
                        &bridge,
                    ),
                    Err(error) => {
                        let _ = events.send(Event::Error {
                            message: error.to_string(),
                            recovery_required: false,
                        });
                    }
                }
            }
            Command::Modify { components } => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                start(
                    &executable,
                    scope,
                    LifecycleAction::Modify,
                    components,
                    None,
                    false,
                    &bridge,
                );
            }
            Command::Repair => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                let components = committed_components(&executable, scope).unwrap_or_default();
                start(
                    &executable,
                    scope,
                    LifecycleAction::Repair { force_files: false },
                    components,
                    None,
                    false,
                    &bridge,
                );
            }
            Command::ConfirmUninstall => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                if zup_windows::is_maintenance_executable(&executable) {
                    confirm_uninstall_from_maintenance_copy(
                        &executable,
                        installed.as_ref(),
                        &events,
                    );
                } else {
                    start(
                        &executable,
                        scope,
                        LifecycleAction::Uninstall,
                        vec![],
                        None,
                        true,
                        &bridge,
                    );
                }
            }
            Command::Retry => {
                if let Some(intent) = bridge.retry.lock().expect("retry state").clone() {
                    start(
                        &executable,
                        intent.scope,
                        intent.action,
                        intent.components,
                        intent.install_directory,
                        intent.cleanup_lock,
                        &bridge,
                    );
                }
            }
        }
    }
}

/// Hand the uninstall to a process that is not the file Windows is deleting.
fn confirm_uninstall_from_maintenance_copy(
    executable: &Path,
    installed: Option<&(SelectedScope, PathBuf, semver::Version)>,
    events: &std::sync::mpsc::Sender<zup_ui::UiEvent>,
) {
    let Some((scope, state_root, _)) = installed else {
        let _ = events.send(zup_ui::UiEvent::Error {
            message: "The installed application was not found".into(),
            recovery_required: false,
        });
        return;
    };
    let app_id =
        match package::open_bundle(executable).and_then(|bundle| package::target_plan(&bundle)) {
            Ok(build) => build.installer.app.id.to_string(),
            Err(error) => {
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: format!("The maintenance package could not be read: {error}"),
                    recovery_required: false,
                });
                return;
            }
        };
    let args = crate::uninstall::UninstallArgs {
        ui: true,
        app_id: Some(app_id),
        scope: Some(match scope {
            SelectedScope::User => crate::cli::ScopeArg::User,
            SelectedScope::Machine => crate::cli::ScopeArg::Machine,
        }),
        state_root: Some(state_root.clone()),
        work_root: None,
        output: crate::cli::OutputArg::Human,
        non_interactive: false,
        yes: true,
    };
    match crate::uninstall::launch_runner(executable, &args, true).map(|_| ()) {
        Ok(()) => {
            let _ = events.send(zup_ui::UiEvent::Quit);
        }
        Err(error) => {
            let _ = events.send(zup_ui::UiEvent::Error {
                message: error.to_string(),
                recovery_required: false,
            });
        }
    }
}

/// The components the committed ledger records.
fn committed_components(
    executable: &Path,
    scope: SelectedScope,
) -> miette::Result<Vec<ComponentId>> {
    let build = package::target_plan(&package::open_bundle(executable)?)?;
    let root = state::resolve_state_root(None, scope)?;
    zup_windows::InstallLedgerStore::new(&root)
        .load(&build.installer.app.id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .map(|ledger| ledger.selected_components)
        .ok_or_else(|| miette::miette!("installation not found"))
}

/// The plan preview a window shows while the person is still choosing.
fn preview(
    executable: &Path,
    scope: SelectedScope,
    selected: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    installed_version: Option<semver::Version>,
    events: &std::sync::mpsc::Sender<zup_ui::UiEvent>,
) -> miette::Result<()> {
    let build = package::target_plan(&package::open_bundle(executable)?)?;
    let installer = &build.installer;
    let action = state::resolve_applied_action(
        installed_version.as_ref(),
        &installer.app.version.to_string(),
    )?;
    let enable = selected.iter().map(ToString::to_string).collect::<Vec<_>>();
    let disable = installer
        .components
        .iter()
        .filter(|component| !component.required && !selected.contains(&component.id))
        .map(|component| component.id.to_string())
        .collect();
    let request = lifecycle::prepare_embedded_transition_with_cancellation(
        Request::Named(action),
        scope,
        None,
        enable,
        disable,
        install_directory,
        lifecycle::EmbeddedPreparationMode {
            // A preview answers "what would this do". It is not the run, so it
            // acquires nothing and it is not cancelled by the run's handle.
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: false,
        },
    )?;
    let mut preview = PlanPreview::from_transaction_plan(&request.request.transaction_plan, scope)
        .with_declared_prerequisites(&installer.prerequisites);
    preview.application = installer.app.name.to_string();
    preview.version = installer.app.version.to_string();
    preview.install_directory = request
        .request
        .transaction_plan
        .install_directory
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    events
        .send(zup_ui::UiEvent::PlanReady(preview))
        .map_err(|error| miette::miette!("preview channel: {error}"))?;
    Ok(())
}

/// Start one lifecycle on its own thread and stream its events to the window.
fn start(
    executable: &Path,
    scope: SelectedScope,
    action: LifecycleAction,
    selected: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    cleanup_lock: bool,
    bridge: &Bridge,
) {
    let cancel = CancellationHandle::new();
    *bridge.cancel.lock().expect("cancel state") = Some(cancel.clone());
    *bridge.retry.lock().expect("retry state") = Some(RetryIntent {
        scope,
        action,
        components: selected.clone(),
        install_directory: install_directory.clone(),
        cleanup_lock,
    });
    let events = bridge.events.clone();
    let log = bridge.log.clone();
    let cancel_slot = bridge.cancel.clone();
    let executable = executable.to_path_buf();
    std::thread::spawn(move || {
        let _ = events.send(zup_ui::UiEvent::Progress {
            completed: 0,
            total: 0,
            action: "Preparing…".into(),
        });
        let prepared = match plan_and_prepare(
            &executable,
            scope,
            action,
            &selected,
            install_directory,
            &cancel,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required: false,
                });
                *cancel_slot.lock().expect("cancel state") = None;
                return;
            }
        };
        let request = prepared.request.clone();
        let drifted = request
            .transaction_plan
            .retired_keys
            .iter()
            .map(|key| format!("{key:?}"))
            .collect::<Vec<_>>();
        let app_id = request.app_id.clone();
        let state_root = request.state_root.clone();
        let (runtime_events, _) = tokio::sync::broadcast::channel(256);
        let mut receiver = runtime_events.subscribe();
        let event_tx = events.clone();
        let pump_log = log.clone();
        let pump = std::thread::spawn(move || {
            loop {
                let event = match receiver.blocking_recv() {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if let RuntimeEvent::LogPath { path } = &event {
                    *pump_log.lock().expect("log state") = Some(path.clone());
                }
                let terminal = execute::is_terminal(&event);
                let _ = event_tx.send(zup_ui::UiEvent::Runtime(event));
                if terminal {
                    break;
                }
            }
        });
        let recovery = request.recovery_id.is_some();
        match execute::execute_with_control(prepared, cancel, runtime_events) {
            Ok(outcome) => {
                if matches!(&outcome, InstallOutcome::Failed(message) if message == "blocked by running applications")
                {
                    *cancel_slot.lock().expect("cancel state") = None;
                    let _ = pump.join();
                    return;
                }
                if matches!(action, LifecycleAction::Repair { .. })
                    && outcome == InstallOutcome::Committed
                {
                    let _ = events.send(zup_ui::UiEvent::RepairFinished {
                        drifted_resources: drifted,
                    });
                }
                if cleanup_lock && outcome == InstallOutcome::Committed {
                    let _ = crate::uninstall::remove_lock(&state_root, &app_id, scope);
                    let _ = zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope);
                }
                let _ = events.send(zup_ui::UiEvent::OperationFinished(outcome));
            }
            Err(error) => {
                let recovery_required = recovery || error.to_string().contains("recovery required");
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required,
                });
            }
        }
        let _ = pump.join();
        *cancel_slot.lock().expect("cancel state") = None;
    });
}

fn plan_and_prepare(
    executable: &Path,
    scope: SelectedScope,
    action: LifecycleAction,
    selected: &[ComponentId],
    install_directory: Option<PathBuf>,
    cancel: &CancellationHandle,
) -> miette::Result<PreparedRuntime> {
    let build = package::target_plan(&package::open_bundle(executable)?)?;
    let enable = selected.iter().map(ToString::to_string).collect::<Vec<_>>();
    let disable = build
        .installer
        .components
        .iter()
        .filter(|component| !component.required && !selected.contains(&component.id))
        .map(|component| component.id.to_string())
        .collect();
    lifecycle::prepare_embedded_transition_with_cancellation(
        Request::Named(action),
        scope,
        None,
        enable,
        disable,
        install_directory,
        lifecycle::EmbeddedPreparationMode {
            cancellation: &lifecycle::RuntimeCancellationQuery(cancel),
            acquire_prerequisites: true,
        },
    )
}

/// Where this process's session log lives when the engine has not named one.
fn session_log_path() -> PathBuf {
    std::env::temp_dir().join("zup-setup.log")
}

fn open_log(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

/// What "Copy diagnostics" puts on the clipboard.
///
/// The session log, and a statement of what is *not* in it. A user pasting a
/// diagnostic into a bug report should be able to say, without reading the code,
/// that no environment variables and no secrets are in the text they are sending.
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
