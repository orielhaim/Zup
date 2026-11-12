use std::path::{Path, PathBuf};
use zup_plan::LifecycleAction;
use zup_preset_host::{HostDecision, HostState, Selection};
use zup_preset_protocol::{Action, UpdateState};
use zup_runtime::{CancellationHandle, OperationControl, OperationHost, RuntimeEvent};

use crate::host::{
    OperationContext, PresetLaunch, SessionDiscovery, SessionHost, SessionLaunch, initial_state,
    is_terminal, select_scope,
};

#[derive(Clone)]
enum Report {
    Engine(uuid::Uuid, RuntimeEvent),
    Plan(
        u64,
        Result<zup_presentation::PlanPreview, zup_runtime::OperationError>,
    ),
    Update(UpdateReport),
}

#[derive(Clone)]
enum UpdateReport {
    Status(uuid::Uuid, UpdateState),
}

type Reports = tokio::sync::broadcast::Sender<Report>;
type Inbox = tokio::sync::broadcast::Receiver<Report>;

struct ActiveOperation<H> {
    source: Option<tokio::sync::oneshot::Receiver<H>>,
    id: uuid::Uuid,
    cancel: CancellationHandle,
    task: tokio::task::JoinHandle<crate::runner::OperationCompletion>,
}

impl<H> ActiveOperation<H> {
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
    active: Option<ActiveOperation<H>>,
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
    let mut deliverable = match publish(&mut preset, &control.state, &configuration) {
        Ok(deliverable) => deliverable,
        Err(error) => return Err(error),
    };
    let mut interaction_error = None;

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
                    Report::Engine(id, event) => {
                        if control.active.as_ref().is_some_and(|active| active.id == id) {
                            control.state.observe(&event);
                        }
                    },
                    Report::Plan(generation, plan) => {
                        if generation != control.plans.generation {
                            continue;
                        }
                        match plan {
                            Ok(plan) => control.state.set_plan(plan),
                            Err(zup_runtime::OperationError::RecoveryRequired(transaction)) => {
                                control.state.plan_failed(format!("Transaction {transaction} requires recovery"));
                                control.state.recovery_required(Some(transaction), "A previous deployment transaction requires recovery before another change can proceed".into());
                            }
                            Err(error) => control.state.plan_failed(error.to_string()),
                        }
                    }
                    Report::Update(UpdateReport::Status(id, status)) => {
                        if control.active.as_ref().is_some_and(|active| active.id == id) {
                            control.state.set_update(None, status);
                        }
                    },
                }
                if let Err(error) = publish_if(&mut preset, &control.state, &configuration, &mut deliverable) {
                    interaction_error = Some(error);
                    break;
                }
            }
            asked = questions.recv() => {
                let Some(action) = asked else { break };
                if action == Action::Close {
                    break;
                }
                act(&mut control, action);
                if let Err(error) = publish_if(&mut preset, &control.state, &configuration, &mut deliverable) {
                    interaction_error = Some(error);
                    break;
                }
            }
            completion = await_active(&mut control.active) => {
                match completion {
                    Ok(completion) => {
                        let replan = completion.kind != crate::runner::CompletionKind::UpdateCheck;
                        apply_completion(&mut control, completion);
                        if replan
                            && control.state.snapshot().state == zup_preset_protocol::InstallerState::Succeeded
                            && let Some(selection) = control.state.plan_request()
                        {
                            request_plan(&mut control, selection);
                        }
                    },
                    Err(error) => {
                        control.active = None;
                        interaction_error = Some(error);
                        break;
                    }
                }
                if let Err(error) = publish_if(&mut preset, &control.state, &configuration, &mut deliverable) {
                    interaction_error = Some(error);
                    break;
                }
            }
        }
    }

    drop(preset);
    let settled = finish_active(&mut control).await;
    match (interaction_error, settled) {
        (Some(error), Err(settled)) => Err(miette::miette!("{error}").wrap_err(settled)),
        (Some(error), Ok(())) => Err(error),
        (None, settled) => settled,
    }
}

async fn await_active<H>(
    active: &mut Option<ActiveOperation<H>>,
) -> Result<crate::runner::OperationCompletion, miette::Report> {
    let Some(operation) = active.as_mut() else {
        std::future::pending().await
    };
    let id = operation.id;
    match (&mut operation.task).await {
        Ok(completion) if completion.id == id => Ok(completion),
        Ok(_) => Err(miette::miette!(
            "the operation completed with a result for a different operation"
        )),
        Err(_) => Err(miette::miette!("the operation task ended unexpectedly")),
    }
}

async fn finish_active<H: SessionHost>(control: &mut Control<H>) -> miette::Result<()> {
    let Some(operation) = control.active.take() else {
        return Ok(());
    };
    operation.cancel();
    match operation.task.await {
        Ok(completion) if completion.id == operation.id => {
            if let Some(mut source) = operation.source
                && let Ok(host) = source.try_recv()
            {
                control.host = host;
            }
            apply_completion(control, completion);
            Ok(())
        }
        Ok(_) => Err(miette::miette!(
            "the operation completed with a result for a different operation"
        )),
        Err(_) => Err(miette::miette!("the operation task ended unexpectedly")),
    }
}

fn publish_if(
    preset: &mut zup_preset_host::process::PresetProcess,
    state: &HostState,
    configuration: &zup_preset_protocol::Configuration,
    deliverable: &mut bool,
) -> miette::Result<()> {
    if *deliverable {
        *deliverable = publish(preset, state, configuration)?;
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
        .is_some_and(|operation| operation.id != completion.id)
    {
        return;
    }
    if let Some(operation) = control.active.take()
        && let Some(mut source) = operation.source
        && let Ok(host) = source.try_recv()
    {
        control.host = host;
    }
    if let Some(status) = completion.update {
        control.state.set_update(None, status);
    }
    let outcome = match completion.outcome {
        crate::SessionOutcome::UpdateChecked => {
            control.state.finish_update_check();
            return;
        }
        crate::SessionOutcome::Deployment(outcome) => outcome,
    };
    if matches!(
        outcome,
        zup_runtime::InstallOutcome::Committed | zup_runtime::InstallOutcome::Recovered { .. }
    ) && completion.kind != crate::runner::CompletionKind::UpdateCheck
    {
        let previous = control
            .discovery
            .installation(control.presented)
            .map(|installation| installation.state_root.clone());
        let refreshed = control
            .host
            .observe(
                control.discovery.installer(),
                control.presented,
                previous.as_deref(),
            )
            .and_then(|context| {
                if let Some(build) = control.host.refresh_installation(&context)? {
                    control.discovery.build = build;
                    control.executable = control.host.content_executable(&control.executable);
                }
                if let Some(installation) = control
                    .discovery
                    .installations
                    .iter_mut()
                    .find(|installation| installation.scope == context.scope)
                {
                    installation.ledger = context.ledger.clone();
                }
                control.state.refresh_installation(initial_state(
                    &control.discovery,
                    &context,
                    None,
                ));
                if completion.kind == crate::runner::CompletionKind::UpdateInstall
                    && let Some(ledger) = context
                        .ledger
                        .as_ref()
                        .filter(|record| record.is_installed())
                {
                    control.state.set_update(
                        None,
                        UpdateState::UpToDate {
                            current: ledger.current.version.to_string(),
                        },
                    );
                }
                Ok(())
            });
        if let Err(error) = refreshed {
            control.state.finish_with(&zup_runtime::InstallOutcome::CommittedPendingReconciliation {
                transaction: control.state.snapshot().recovery_transaction.map(|id| id.0),
                detail: format!("The operation settled, but the installed state could not be refreshed: {error}"),
            });
            return;
        }
    }
    if !completion.repair_drift.is_empty() {
        control.state.set_repair_drift(completion.repair_drift);
    }
    control.state.finish_with(&outcome);
    if matches!(
        completion.kind,
        crate::runner::CompletionKind::UpdateCheck | crate::runner::CompletionKind::UpdateInstall
    ) && !matches!(outcome, zup_runtime::InstallOutcome::Committed)
    {
        let status = if matches!(outcome, zup_runtime::InstallOutcome::Cancelled) {
            UpdateState::Idle
        } else {
            let message = control
                .state
                .snapshot()
                .diagnostic
                .as_ref()
                .map(|diagnostic| diagnostic.meaning.clone())
                .unwrap_or_else(|| "The update did not complete".into());
            UpdateState::Failed { message }
        };
        control.state.set_update(None, status);
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
    H: SessionHost + Clone + Send + Sync + 'static,
{
    control.plans.generation += 1;
    let generation = control.plans.generation;
    let reports = control.reports.clone();
    let host = control.host.clone();
    let discovery = control.discovery.clone();
    let executable = control.executable.clone();
    if let Err(error) = spawn("setup-plan", move || {
        let plan = preview(&host, &executable, &discovery, &selection);
        let _ = reports.send(Report::Plan(generation, plan));
    }) {
        control.state.plan_failed(error);
    }
}

fn act<H>(control: &mut Control<H>, action: Action)
where
    H: SessionHost + Clone + Send + Sync + 'static,
{
    let decision = control.state.accept(action);
    if matches!(
        decision,
        HostDecision::Run { .. } | HostDecision::Recover { .. } | HostDecision::Update(_)
    ) {
        control.plans.generation += 1;
    }
    match decision {
        HostDecision::Run { action, selection } => {
            control.inbox = control.reports.subscribe();
            if control.active.is_some() {
                return;
            }
            let operation_control =
                OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
            let cancel = operation_control.cancellation();
            let id = uuid::Uuid::now_v7();
            let operation = OperationRequest {
                id,
                action,
                selection,
            };
            let host = control.host.clone();
            let executable = control.executable.clone();
            let discovery = control.discovery.clone();
            let reports = control.reports.clone();
            let task = tokio::spawn(settle_operation(
                host,
                executable,
                discovery,
                operation,
                operation_control,
                reports,
            ));
            control.active = Some(ActiveOperation {
                id,
                cancel,
                task,
                source: None,
            });
        }
        HostDecision::Plan(selection) => {
            if selection.scope != control.presented {
                rebind(control, selection.scope);
            } else {
                request_plan(control, selection);
            }
        }
        HostDecision::Recover { transaction } => {
            if control.active.is_some() {
                return;
            }
            let scope =
                zup_preset_host::convert::engine_scope(control.state.snapshot().surface.scope());
            let context = match control.discovery.context(scope) {
                Ok(context) => context,
                Err(error) => {
                    control
                        .state
                        .fail(error, zup_presentation::ProcessOutcome::Failure);
                    return;
                }
            };
            let operation = OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
            let cancel = operation.cancellation();
            let received = operation.subscribe();
            let id = uuid::Uuid::now_v7();
            let host = control.host.clone();
            let work = async move {
                let outcome = zup_runtime::outcome_of(
                    host.recover(&context, transaction, Default::default(), &operation)
                        .await,
                );
                crate::runner::OperationCompletion {
                    id,
                    outcome: crate::SessionOutcome::Deployment(outcome),
                    kind: crate::runner::CompletionKind::Recovery,
                    update: None,
                    repair_drift: Vec::new(),
                }
            };
            let task = tokio::spawn(report_operation(
                id,
                received,
                control.reports.clone(),
                work,
            ));
            control.active = Some(ActiveOperation {
                id,
                cancel,
                task,
                source: None,
            });
        }
        HostDecision::Update(request) => {
            if control.active.is_some() {
                return;
            }
            let scope =
                zup_preset_host::convert::engine_scope(control.state.snapshot().surface.scope());
            let context = match control.discovery.context(scope) {
                Ok(context) => context,
                Err(error) => {
                    control
                        .state
                        .fail(error, zup_presentation::ProcessOutcome::Failure);
                    return;
                }
            };
            let sender = control.reports.clone();
            let mut host = control.host.clone();
            let (source, received_source) = tokio::sync::oneshot::channel();
            let operation = OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
            let cancel = operation.cancellation();
            let id = uuid::Uuid::now_v7();
            let mut events = operation.subscribe();
            let kind = if matches!(request, zup_runtime::UpdateRequest::Check) {
                crate::runner::CompletionKind::UpdateCheck
            } else {
                crate::runner::CompletionKind::UpdateInstall
            };
            let task = tokio::spawn(async move {
                let status_sender = sender.clone();
                let mut work = tokio::task::spawn_blocking(move || {
                    let mut final_status = None;
                    let mut status = |progress| {
                        if matches!(
                            progress,
                            UpdateState::Available { .. } | UpdateState::UpToDate { .. }
                        ) {
                            final_status = Some(progress);
                        } else {
                            let _ = status_sender
                                .send(Report::Update(UpdateReport::Status(id, progress)));
                        }
                    };
                    let result = host.update(&context, &request, &operation, &mut status);
                    let _ = source.send(host);
                    (result, final_status)
                });
                let result = loop {
                    tokio::select! {
                        result = &mut work => break result,
                        event = events.recv() => if let Ok(event) = event && !is_terminal(&event) { let _ = sender.send(Report::Engine(id, event)); },
                    }
                };
                let (outcome, update) = match result {
                    Ok((Ok(outcome), status)) => (outcome, status),
                    Ok((Err(error), _)) => (
                        crate::SessionOutcome::Deployment(zup_runtime::InstallOutcome::Failed(
                            error,
                        )),
                        None,
                    ),
                    Err(error) => (
                        crate::SessionOutcome::Deployment(zup_runtime::InstallOutcome::Failed(
                            error.to_string(),
                        )),
                        None,
                    ),
                };
                crate::runner::OperationCompletion {
                    update,
                    kind,
                    id,
                    outcome,
                    repair_drift: Vec::new(),
                }
            });
            control.active = Some(ActiveOperation {
                id,
                cancel,
                task,
                source: Some(received_source),
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
                control.state.fail(
                    format!("open the session log: {error}"),
                    zup_presentation::ProcessOutcome::Failure,
                );
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
    id: uuid::Uuid,
    action: LifecycleAction,
    selection: Selection,
}

fn rebind<H: SessionHost + Clone + Send + Sync + 'static>(
    control: &mut Control<H>,
    scope: zup_core::SelectedScope,
) {
    let context = match control.discovery.context(scope) {
        Ok(context) => context,
        Err(message) => {
            control
                .state
                .fail(message, zup_presentation::ProcessOutcome::Failure);
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
    let received = control.subscribe();
    let work = crate::runner::complete_operation(
        &host,
        &executable,
        &discovery,
        crate::runner::OperationLaunch {
            id: operation.id,
            action: operation.action,
            selection: &operation.selection,
            control,
        },
    );
    report_operation(operation.id, received, reports, work).await
}

async fn report_operation(
    id: uuid::Uuid,
    mut received: tokio::sync::broadcast::Receiver<RuntimeEvent>,
    reports: Reports,
    work: impl std::future::Future<Output = crate::runner::OperationCompletion>,
) -> crate::runner::OperationCompletion {
    tokio::pin!(work);
    let completion = loop {
        tokio::select! {
            biased;
            done = &mut work => break done,
            event = received.recv() => {
                match event {
                    Ok(event) if !is_terminal(&event) => {
                        if reports.send(Report::Engine(id, event)).is_err() {
                            break work.await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break work.await,
                }
            }
        }
    };
    loop {
        match received.try_recv() {
            Ok(event) if !is_terminal(&event) => {
                if reports.send(Report::Engine(id, event)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }
    completion
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
    executable: &Path,
    discovery: &SessionDiscovery,
    selection: &Selection,
) -> Result<zup_presentation::PlanPreview, zup_runtime::OperationError> {
    let context = discovery
        .context(selection.scope)
        .map_err(|error| zup_runtime::OperationError::refused("context", error))?;
    let bound = host
        .bind(&context)
        .map_err(|error| zup_runtime::OperationError::refused("planning", error))?;
    let (enable, disable) = crate::host::selection_of(discovery.installer(), selection);
    let prepared = zup_runtime::prepare_operation(
        &zup_runtime::OperationInput {
            build: &discovery.build,
            installer: &host.content_executable(executable),
            scope: selection.scope,
            operation: zup_runtime::Operation::Apply,
            selection: zup_runtime::OperationSelection {
                deployment: None,
                enable,
                disable,
                install_directory: selection.install_directory.as_deref().map(PathBuf::from),
            },
            prior: context.ledger.as_ref(),
            state_root: &context.state_root,
            cancellation: &zup_plan::NeverCancelled,
        },
        &bound,
    )?;
    zup_presentation::PlanPreview::from_transaction_plan(
        &prepared.transaction_plan,
        selection.scope,
        prepared.target_plan(),
    )
    .map_err(|error| zup_runtime::OperationError::refused("preview", error))
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
        zup_plan::DesiredState,
        zup_core::Installer,
        zup_plan::BoundDeployment,
    ),
    String,
> {
    let bound = host.bind(context)?;
    let installer = discovery.build.installer.clone();
    let action = zup_runtime::resolve_operation(
        zup_runtime::Operation::Apply,
        context
            .ledger
            .as_ref()
            .map(|ledger| &ledger.current.version),
        &installer.app.version,
    )
    .map_err(|error| error.to_string())?;
    let (enable, disable) = crate::host::selection_of(&installer, selection);
    let operation_selection = zup_runtime::OperationSelection {
        deployment: None,
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
