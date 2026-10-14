use std::path::{Path, PathBuf};
use zup_exec::LifecycleAction;
use zup_preset_host::{HostDecision, HostState, Selection};
use zup_preset_protocol::{Action, UpdateState};
use zup_runtime::{CancellationHandle, OperationControl, OperationHost, RuntimeEvent};

use crate::host::{
    OperationContext, PresetLaunch, SessionDiscovery, SessionHost, SessionLaunch, initial_state,
    is_terminal, select_scope,
};

#[derive(Clone)]
enum Report {
    Engine(RuntimeEvent),
    Plan(u64, Result<zup_presentation::PlanPreview, String>),
    Update(UpdateReport),
}

#[derive(Clone)]
enum UpdateReport {
    Status(UpdateState),
    Finished(Result<(), String>),
}

type Reports = tokio::sync::broadcast::Sender<Report>;
type Inbox = tokio::sync::broadcast::Receiver<Report>;

struct ActiveOperation {
    id: zup_runtime::Uuid,
    cancel: CancellationHandle,
}

impl ActiveOperation {
    fn cancel(&self) {
        self.cancel.cancel();
    }
}

struct Control<H: SessionHost> {
    host: H,
    executable: PathBuf,
    state: HostState,
    discovery: SessionDiscovery,
    presented: zup_core::SelectedScope,
    active: Option<ActiveOperation>,
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
    H: SessionHost + Clone + Send + Sync + 'static,
{
    let discovery = host
        .discover(executable, &launch)
        .map_err(|error| miette::miette!("{error}"))?;
    let presented = select_scope(
        discovery.installer().install.scope,
        &launch,
        &discovery.installations,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    let context = discovery
        .context(presented)
        .map_err(|error| miette::miette!("{error}"))?;
    let state = initial_state(&discovery, &context, launch.preselected.as_deref());
    let PresetLaunch {
        executable: preset_exe,
        configuration,
        capabilities,
        product,
        environment,
        stage: _stage,
    } = host
        .materialize_preset(&discovery, &context)
        .map_err(|error| miette::miette!("{error}"))?;

    let mut preset =
        zup_preset_host::process::launch(&preset_exe, capabilities, product, environment)
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

    let (reports, inbox) = tokio::sync::broadcast::channel::<Report>(256);
    let (settled_tx, mut settled_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::runner::OperationCompletion>();
    let mut control = Control {
        host,
        executable: executable.to_path_buf(),
        state,
        discovery,
        presented,
        active: None,
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
                let report = match report {
                    Ok(report) => report,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                match report {
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
                act(&mut control, action, &settled_tx);
                if deliverable {
                    deliverable = publish(&mut preset, &control.state, &configuration)?;
                }
            }
            completion = settled_rx.recv() => {
                let Some(completion) = completion else { break };
                apply_completion(&mut control, completion);
                if deliverable {
                    deliverable = publish(&mut preset, &control.state, &configuration)?;
                }
            }
        }
    }

    let pending = control.active.take();
    if let Some(operation) = &pending {
        operation.cancel();
    }
    drop(preset);
    if let Some(operation) = pending {
        loop {
            let Some(completion) = settled_rx.recv().await else {
                return Err(miette::miette!(
                    "the operation ended without a settlement result"
                ));
            };
            if completion.id == operation.id {
                apply_completion(&mut control, completion);
                break;
            }
        }
    }
    Ok(())
}

fn apply_completion<H: SessionHost>(
    control: &mut Control<H>,
    completion: crate::runner::OperationCompletion,
) {
    if control
        .active
        .as_ref()
        .is_some_and(|operation| operation.id == completion.id)
    {
        control.active = None;
    }
    if !completion.repair_drift.is_empty() {
        control.state.set_repair_drift(completion.repair_drift);
    }
    control.state.finish_with(&completion.outcome);
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
    H: SessionHost + Clone + Send + Sync + 'static,
{
    control.plans.generation += 1;
    let generation = control.plans.generation;
    let reports = control.reports.clone();
    let host = control.host.clone();
    let discovery = control.discovery.clone();
    let _ = spawn("setup-plan", move || {
        let plan = preview(&host, &discovery, &selection);
        let _ = reports.send(Report::Plan(generation, plan));
    });
}

fn act<H>(
    control: &mut Control<H>,
    action: Action,
    settled: &tokio::sync::mpsc::UnboundedSender<crate::runner::OperationCompletion>,
) where
    H: SessionHost + Clone + Send + Sync + 'static,
{
    match control.state.accept(action) {
        HostDecision::Run {
            action,
            selection,
            cleanup_lock,
        } => {
            control.inbox = control.reports.subscribe();
            let operation_control =
                OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
            let cancel = operation_control.cancellation();
            let id = zup_runtime::Uuid::now_v7();
            control.active = Some(ActiveOperation { id, cancel });
            let operation = OperationRequest {
                id,
                action,
                selection,
                cleanup_lock,
            };
            let host = control.host.clone();
            let executable = control.executable.clone();
            let discovery = control.discovery.clone();
            let reports = control.reports.clone();
            let settled = settled.clone();
            tokio::spawn(async move {
                let done = settle_operation(
                    host,
                    executable,
                    discovery,
                    operation,
                    operation_control,
                    reports,
                )
                .await;
                let _ = settled.send(done);
            });
        }
        HostDecision::Plan(selection) => {
            if selection.scope != control.presented {
                rebind(control, selection.scope);
            } else {
                request_plan(control, selection);
            }
        }
        HostDecision::Update => {
            let scope = zup_preset_host::engine_scope(control.state.snapshot().surface.scope());
            let sender = control.reports.clone();
            let host = control.host.clone();
            let _ = spawn("setup-update", move || {
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
            let _ = spawn("setup-launch", move || {
                let started = launch_target(&host, &discovery, &selection, &target.name);
                if let Err(error) = started {
                    eprintln!("{} did not start: {error}", target.name);
                }
            });
        }
        HostDecision::Cancel => {
            if let Some(operation) = control.active.as_ref() {
                operation.cancel();
            }
        }
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
    id: zup_runtime::Uuid,
    action: LifecycleAction,
    selection: Selection,
    cleanup_lock: bool,
}

fn rebind<H: SessionHost + Clone + Send + Sync + 'static>(
    control: &mut Control<H>,
    scope: zup_core::SelectedScope,
) {
    let context = match control.discovery.context(scope) {
        Ok(context) => context,
        Err(message) => {
            control.state.fail(message, false);
            return;
        }
    };
    let log = control.state.log_path().map(str::to_owned);
    let mut state = initial_state(&control.discovery, &context, None);
    if let Some(log) = log {
        state.set_log_path(log);
    }
    control.state = state;
    control.presented = scope;
    if let Some(selection) = control.state.plan_request() {
        request_plan(control, selection);
    }
}

async fn settle_operation<H>(
    host: H,
    executable: PathBuf,
    discovery: SessionDiscovery,
    operation: OperationRequest,
    control: OperationControl,
    reports: Reports,
) -> crate::runner::OperationCompletion
where
    H: SessionHost + Send + Sync + 'static,
{
    let id = operation.id;
    let mut received = control.subscribe();
    let pump = tokio::spawn(async move {
        loop {
            match received.recv().await {
                Ok(event) if !is_terminal(&event) => {
                    if reports.send(Report::Engine(event)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let joined = tokio::spawn(async move {
        crate::runner::complete_operation(
            &host,
            &executable,
            &discovery,
            crate::runner::OperationLaunch {
                id: operation.id,
                action: operation.action,
                selection: &operation.selection,
                cleanup_lock: operation.cleanup_lock,
                control,
            },
        )
        .await
    })
    .await;
    let _ = pump.await;
    match joined {
        Ok(completion) => completion,
        Err(_) => crate::runner::OperationCompletion {
            id,
            outcome: zup_runtime::InstallOutcome::RecoveryRequired,
            repair_drift: Vec::new(),
        },
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(body)
        .map(drop)
        .map_err(|error| error.to_string())
}

fn preview<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    selection: &Selection,
) -> Result<zup_presentation::PlanPreview, String> {
    let context = discovery.context(selection.scope)?;
    let (plan, installer, target) = target_plan(host, discovery, &context, selection)?;
    let mut preview = zup_presentation::PlanPreview::from_install_plan(&plan)
        .with_declared_prerequisites(&installer.prerequisites);
    preview.install_directory = target.install_directory.to_string();
    preview.estimated_bytes = target.summary.install_bytes;
    preview.requires_authorization = target.summary.requires_authorization;
    Ok(preview)
}

fn launch_target<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    selection: &Selection,
    name: &str,
) -> Result<(), String> {
    let context = discovery.context(selection.scope)?;
    let (_, _, target) = target_plan(host, discovery, &context, selection)?;
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
    context: &OperationContext,
    selection: &Selection,
) -> Result<
    (
        zup_plan::InstallPlan,
        zup_core::Installer,
        zup_platform::TargetPlan,
    ),
    String,
> {
    let bound = host.bind(context)?;
    let installer = discovery.build.installer.clone();
    let action = zup_runtime::resolve_operation(
        zup_runtime::Operation::Apply,
        context.ledger.as_ref().map(|ledger| &ledger.version),
        &installer.app.version,
    )
    .map_err(|error| error.to_string())?;
    let (enable, disable) = crate::host::selection_of(&installer, selection);
    let operation_selection = zup_runtime::OperationSelection {
        enable,
        disable,
        install_directory: selection.install_directory.as_deref().map(PathBuf::from),
    };
    let directory = bound.install_directory(
        operation_selection.install_directory.as_deref(),
        context.ledger.as_ref(),
        installer.install.allow_directory_override,
        matches!(action, LifecycleAction::Repair { .. }),
    )?;
    let request = zup_runtime::plan_request(
        &installer,
        selection.scope,
        context.ledger.as_ref(),
        &operation_selection,
        action,
        directory,
    )
    .map_err(|error| error.to_string())?;
    let plan = bound
        .plan_build(&discovery.build, &request, &zup_plan::NeverCancelled)
        .map_err(|error| error.to_string())?;
    let target = bound.resolve_target(&plan.plan)?;
    Ok((plan.plan, installer, target))
}

fn diagnostic_summary(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "zup setup diagnostic\nlog: {path}\nNo environment variables or secrets are included."
        ),
        None => "zup setup diagnostic\nNo session log is available yet.".into(),
    }
}
