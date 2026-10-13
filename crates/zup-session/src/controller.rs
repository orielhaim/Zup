use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zup_exec::LifecycleAction;
use zup_preset_host::{HostDecision, HostState, Selection};
use zup_preset_protocol::{Action, UpdateState};
use zup_runtime::{CancellationHandle, InstallOutcome, RuntimeEvent};

use crate::host::{
    PresetLaunch, SessionDiscovery, SessionHost, SessionLaunch, initial_state, is_terminal,
};

#[derive(Clone)]
enum Report {
    Engine(RuntimeEvent),
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

struct Control<H: SessionHost> {
    host: H,
    executable: PathBuf,
    state: HostState,
    discovery: SessionDiscovery,
    active: Arc<Mutex<Active>>,
    reports: Reports,
    inbox: Inbox,
    plans: Plans,
}

#[derive(Default)]
struct Plans {
    generation: u64,
}

pub async fn run<H>(mut host: H, executable: &Path, launch: SessionLaunch) -> miette::Result<()>
where
    H: SessionHost + Clone + Send + 'static,
{
    let discovery = host
        .discover(executable, &launch)
        .map_err(|error| miette::miette!("{error}"))?;
    let state = initial_state(&discovery, &launch);
    let PresetLaunch {
        executable: preset_exe,
        configuration,
        capabilities,
        product,
    } = host
        .materialize_preset(&discovery)
        .map_err(|error| miette::miette!("{error}"))?;

    let mut preset = zup_preset_host::process::launch(&preset_exe, capabilities, product)
        .map_err(|error| miette::miette!("{error}"))?;

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
    let (reports, inbox) = tokio::sync::broadcast::channel::<Report>(256);
    let mut control = Control {
        host,
        executable: executable.to_path_buf(),
        state,
        discovery,
        active,
        reports,
        inbox,
        plans: Plans::default(),
    };

    if launch.auto_uninstall {
        control.state.accept(Action::RequestUninstall);
    }
    if let Some(selection) = control.state.plan_request() {
        request_plan(&mut control, selection);
    }
    let mut deliverable = publish(&mut preset, &control.state, &configuration)?;

    loop {
        if !preset.is_running() {
            break;
        }
        tokio::select! {
            biased;
            report = control.inbox.recv() => {
                match report.map_err(lagged)? {
                    Report::Engine(event) => control.state.observe(&event),
                    Report::Plan(generation, plan) => {
                        if generation != control.plans.generation {
                            continue;
                        }
                        match plan {
                            Ok(plan) => control.state.set_plan(plan),
                            Err(message) => control.state.plan_failed(message),
                        }
                    }
                    Report::Update(UpdateReport::Status(status)) => control.state.set_update(None, status),
                    Report::Update(UpdateReport::Finished(Ok(()))) => {}
                    Report::Update(UpdateReport::Finished(Err(message))) => {
                        control.state.fail(message, false);
                    }
                    Report::RepairDrift(resources) => control.state.set_repair_drift(resources),
                }
                if deliverable {
                    deliverable = publish(&mut preset, &control.state, &configuration)?;
                }
            }
            asked = questions.recv() => {
                let Some(action) = asked else { break };
                if action == Action::Close {
                    break;
                }
                act(&mut control, action);
                if deliverable {
                    deliverable = publish(&mut preset, &control.state, &configuration)?;
                }
            }
        }
    }

    control
        .active
        .lock()
        .expect("the running operation")
        .cancel();
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

fn publish(
    preset: &mut zup_preset_host::process::PresetProcess,
    state: &HostState,
    configuration: &zup_preset_protocol::Configuration,
) -> miette::Result<bool> {
    match preset.publish(configuration.clone(), Box::new(state.snapshot().clone())) {
        Ok(()) => Ok(true),
        Err(zup_preset_host::process::SessionError::Disconnected(_)) => Ok(false),
        Err(error) => Err(miette::miette!("{error}")),
    }
}

fn request_plan<H>(control: &mut Control<H>, selection: Selection)
where
    H: SessionHost + Clone + Send + 'static,
{
    control.plans.generation += 1;
    let generation = control.plans.generation;
    let reports = control.reports.clone();
    let host = control.host.clone();
    let discovery = control.discovery.clone();
    spawn("setup-plan", move || {
        let plan = preview(&host, &discovery, &selection);
        let _ = reports.send(Report::Plan(generation, plan));
    });
}

fn act<H>(control: &mut Control<H>, action: Action)
where
    H: SessionHost + Clone + Send + 'static,
{
    match control.state.accept(action) {
        HostDecision::Run {
            action,
            selection,
            cleanup_lock,
        } => {
            control.inbox = control.reports.subscribe();
            let cancel = CancellationHandle::new();
            control
                .active
                .lock()
                .expect("the running operation")
                .set(cancel.clone());
            let operation = OperationRequest {
                action,
                selection,
                cleanup_lock,
            };
            let host = control.host.clone();
            let executable = control.executable.clone();
            let discovery = control.discovery.clone();
            let reports = control.reports.clone();
            spawn("setup-lifecycle", move || {
                run_lifecycle(host, executable, discovery, operation, cancel, reports);
            });
        }
        HostDecision::Plan(selection) => request_plan(control, selection),
        HostDecision::Update => {
            let scope = control.discovery.scope;
            let sender = control.reports.clone();
            let host = control.host.clone();
            spawn("setup-update", move || {
                let mut status = |progress| {
                    let _ = sender.send(Report::Update(UpdateReport::Status(progress)));
                };
                let result = host.update(scope, &mut status);
                let _ = sender.send(Report::Update(UpdateReport::Finished(result)));
            });
        }
        HostDecision::Launch(target) => {
            let selection = Selection::from_surface(&control.state.snapshot().surface);
            let host = control.host.clone();
            let discovery = control.discovery.clone();
            spawn("setup-launch", move || {
                let started = launch_target(&host, &discovery, &selection, &target.name);
                if let Err(error) = started {
                    eprintln!("{} did not start: {error}", target.name);
                }
            });
        }
        HostDecision::Cancel => control
            .active
            .lock()
            .expect("the running operation")
            .cancel(),
        HostDecision::OpenLog => {
            if let Some(path) = control.state.log_path()
                && let Err(error) = control.host.open_log(path)
            {
                control
                    .state
                    .fail(format!("open the session log: {error}"), false);
            }
        }
        HostDecision::CopyDiagnostics => {
            control
                .host
                .copy_diagnostics(&diagnostic_summary(control.state.log_path()));
        }
        HostDecision::Acknowledged | HostDecision::Refused(_) => {}
    }
}

struct OperationRequest {
    action: LifecycleAction,
    selection: Selection,
    cleanup_lock: bool,
}

struct CancelQuery<'a>(&'a CancellationHandle);

impl zup_plan::CancellationQuery for CancelQuery<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

fn run_lifecycle<H>(
    host: H,
    executable: PathBuf,
    discovery: SessionDiscovery,
    operation: OperationRequest,
    cancel: CancellationHandle,
    reports: Reports,
) where
    H: SessionHost,
{
    let (enable, disable) =
        crate::host::selection_of(&discovery.build.installer, &operation.selection);
    let query = CancelQuery(&cancel);
    let input = zup_runtime::OperationInput {
        build: &discovery.build,
        installer: &executable,
        scope: operation.selection.scope,
        operation: zup_runtime::Operation::from(operation.action),
        selection: zup_runtime::OperationSelection {
            enable,
            disable,
            install_directory: operation
                .selection
                .install_directory
                .as_deref()
                .map(PathBuf::from),
        },
        prior: discovery
            .priors
            .iter()
            .find(|(scope, _)| *scope == operation.selection.scope)
            .map(|(_, ledger)| ledger),
        state_root: &discovery.state_root,
        cancellation: &query,
    };
    let prepared = match zup_runtime::prepare_operation(&input, &host) {
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
                let terminal = is_terminal(&event);
                if reports.send(Report::Engine(event)).is_err() || terminal {
                    break;
                }
            }
        })
    };
    let control =
        zup_runtime::RuntimeControl::new(cancel, engine, zup_runtime::ExecutionPolicy::Interactive);
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime
                .block_on(crate::runner::run_operation(
                    &host, &discovery, &prepared, control,
                ))
                .map_err(|error| error.to_string())
        });
    let _ = pump.join();

    let outcome: Result<InstallOutcome, String> = outcome;
    if matches!(operation.action, LifecycleAction::Repair { .. })
        && matches!(outcome, Ok(InstallOutcome::Committed))
        && !drifted.is_empty()
    {
        let _ = reports.send(Report::RepairDrift(drifted));
    }
    if operation.cleanup_lock
        && matches!(outcome, Ok(InstallOutcome::Committed))
        && let Err(error) = host.after_commit(&discovery, &prepared)
    {
        let _ = reports.send(Report::Engine(RuntimeEvent::Failed {
            kind: "commit_failed".into(),
            message: error,
        }));
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(body);
}

fn preview<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    selection: &Selection,
) -> Result<zup_presentation::PlanPreview, String> {
    let (plan, installer) = target_plan(host, discovery, selection)?;
    let mut preview = zup_presentation::PlanPreview::from_install_plan(&plan)
        .with_declared_prerequisites(&installer.prerequisites);
    if let Ok(target) = host.resolve_target(&plan) {
        preview.install_directory = target.install_directory.to_string();
        preview.estimated_bytes = target.summary.install_bytes;
        preview.requires_authorization = target.summary.requires_authorization;
    }
    Ok(preview)
}

fn launch_target<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    selection: &Selection,
    name: &str,
) -> Result<(), String> {
    let (plan, _) = target_plan(host, discovery, selection)?;
    let target = host
        .resolve_target(&plan)
        .map_err(|error| error.to_string())?;
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
    command.spawn().map(drop).map_err(|error| error.to_string())
}

fn target_plan<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    selection: &Selection,
) -> Result<(zup_plan::InstallPlan, zup_core::Installer), String> {
    let installer = discovery.build.installer.clone();
    let mut request = zup_plan::PlanRequest::new(installer.target, selection.scope);
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
        .map(zup_runtime::install_directory_template)
        .transpose()
        .map_err(|error| error.to_string())?;

    let build = zup_core::BuildPlan {
        targets: vec![discovery.build.clone()],
    };
    let plan = host
        .plan_build(&build.targets[0], &request, &zup_plan::NeverCancelled)
        .map_err(|error| error.to_string())?;
    Ok((plan.plan, installer))
}

fn diagnostic_summary(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "zup setup diagnostic\nlog: {path}\nNo environment variables or secrets are included."
        ),
        None => "zup setup diagnostic\nNo session log is available yet.".into(),
    }
}
