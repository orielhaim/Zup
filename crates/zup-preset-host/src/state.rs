use zup_core::SelectedScope;
use zup_exec::LifecycleAction;
use zup_preset_protocol::{
    Action, Capabilities, Capability, ComponentId, DiagnosticKind, DiagnosticPresentation,
    InstallScope, InstallerState, LaunchTarget, MaintenanceState, OperationKind, OperationPhase,
    PlanStatus, ProductIdentity, ProgressPresentation, Snapshot, Surface, UpdatePresentation,
    UpdateState,
};
use zup_runtime::{InstallOutcome, RuntimeEvent};

use crate::convert;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub scope: SelectedScope,
    pub components: Vec<zup_core::ComponentId>,
    pub install_directory: Option<String>,
}

impl Selection {
    pub fn from_surface(surface: &Surface) -> Self {
        Self {
            scope: convert::engine_scope(surface.scope()),
            components: surface
                .components()
                .iter()
                .filter(|component| component.selected)
                .map(|component| zup_core::ComponentId::new(component.id.as_str()))
                .collect::<Result<Vec<_>, _>>()
                .expect("a protocol component id came from an engine component id"),
            install_directory: surface.install_directory().map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launchable {
    pub target: LaunchTarget,
    pub component: Option<ComponentId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDecision {
    Run {
        action: LifecycleAction,
        selection: Selection,
        cleanup_lock: bool,
    },
    Plan(Selection),
    Update,
    Cancel,
    OpenLog,
    CopyDiagnostics,
    Launch(LaunchTarget),
    Acknowledged,
    Refused(ActionRefusal),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionRefusal {
    #[error("this surface does not offer that operation")]
    UnsupportedSurface,
    #[error("an operation is already running")]
    Busy,
    #[error("`{0}` is not a component of this application")]
    UnknownComponent(String),
    #[error("`{0}` is required and cannot be turned off")]
    RequiredComponent(String),
    #[error("this application does not allow choosing an install location")]
    DirectoryNotAllowed,
    #[error("this application does not support that scope")]
    UnsupportedScope,
    #[error("there is nothing to retry")]
    NothingToRetry,
    #[error("the operation is not in a state that can be cancelled")]
    NothingToCancel,
    #[error("no uninstall is waiting to be confirmed")]
    NothingToConfirm,
    #[error("there is no session log to open yet")]
    NoLog,
    #[error("there is nothing installed to start")]
    NothingToLaunch,
}

pub struct HostState {
    snapshot: Snapshot,
    capabilities: Capabilities,
    retry: Option<(LifecycleAction, Selection)>,
    log_path: Option<String>,
    running: bool,
    launchers: Vec<Launchable>,
}

impl HostState {
    pub fn install(
        product: ProductIdentity,
        options: zup_preset_protocol::InstallOptions,
        capabilities: Capabilities,
    ) -> Self {
        Self::open(product, Surface::Install(options), capabilities)
    }

    pub fn maintenance(
        product: ProductIdentity,
        state: MaintenanceState,
        capabilities: Capabilities,
    ) -> Self {
        Self::open(product, Surface::Maintenance(state), capabilities)
    }

    fn open(product: ProductIdentity, surface: Surface, capabilities: Capabilities) -> Self {
        let state = match &surface {
            Surface::Install(_) => InstallerState::Options,
            Surface::Maintenance(_) => InstallerState::Maintenance,
        };
        let plan = if capabilities.contains(Capability::PlanPreview) {
            PlanStatus::Computing { last: None }
        } else {
            PlanStatus::Unsupported
        };
        Self {
            snapshot: Snapshot {
                product,
                surface,
                state,
                operation: None,
                progress: None,
                plan,
                diagnostic: None,
                update: None,
                repair_drift: Vec::new(),
                launch: None,
            },
            capabilities,
            retry: None,
            log_path: None,
            running: false,
            launchers: Vec::new(),
        }
    }

    pub fn with_launchers(mut self, launchers: Vec<Launchable>) -> Self {
        self.launchers = launchers;
        self
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn log_path(&self) -> Option<&str> {
        self.log_path.as_deref()
    }

    fn resting_state(&self) -> InstallerState {
        match &self.snapshot.surface {
            Surface::Install(_) => InstallerState::Options,
            Surface::Maintenance(_) => InstallerState::Maintenance,
        }
    }

    pub fn accept(&mut self, action: Action) -> HostDecision {
        match action {
            Action::SetScope { scope } => self.set_scope(scope),
            Action::SetComponent {
                component,
                selected,
            } => self.set_component(&component, selected),
            Action::SetInstallDirectory { directory } => {
                self.set_install_directory(Some(directory))
            }
            Action::ResetInstallDirectory => self.set_install_directory(None),
            Action::Install => self.start(LifecycleAction::Install, false),
            Action::Update => self.update(),
            Action::Modify => self.maintenance_op(LifecycleAction::Modify, false),
            Action::Repair => {
                self.maintenance_op(LifecycleAction::Repair { force_files: false }, false)
            }
            Action::RequestUninstall => self.request_uninstall(),
            Action::ConfirmUninstall => self.confirm_uninstall(),
            Action::DismissUninstall => self.dismiss_uninstall(),
            Action::Cancel => self.cancel(),
            Action::Retry => self.retry(),
            Action::OpenLog => {
                if self.log_path.is_some() {
                    HostDecision::OpenLog
                } else {
                    HostDecision::Refused(ActionRefusal::NoLog)
                }
            }
            Action::CopyDiagnostics => HostDecision::CopyDiagnostics,
            Action::Launch => match &self.snapshot.launch {
                Some(target) => HostDecision::Launch(target.clone()),
                None => HostDecision::Refused(ActionRefusal::NothingToLaunch),
            },
            Action::Close => HostDecision::Acknowledged,
        }
    }

    pub fn plan_request(&mut self) -> Option<Selection> {
        if !self.capabilities.contains(Capability::PlanPreview) {
            return None;
        }
        let last = self.snapshot.plan.latest().cloned().map(Box::new);
        self.snapshot.plan = PlanStatus::Computing { last };
        Some(Selection::from_surface(&self.snapshot.surface))
    }

    fn replan(&mut self) -> HostDecision {
        match self.plan_request() {
            Some(selection) => HostDecision::Plan(selection),
            None => HostDecision::Acknowledged,
        }
    }

    fn set_scope(&mut self, scope: InstallScope) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        if !self.snapshot.surface.scopes().contains(&scope) {
            return HostDecision::Refused(ActionRefusal::UnsupportedScope);
        }
        if self.snapshot.surface.scope() == scope {
            return HostDecision::Acknowledged;
        }
        self.snapshot.surface.set_scope(scope);
        self.replan()
    }

    fn set_component(&mut self, id: &ComponentId, selected: bool) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        let components = self.snapshot.surface.components_mut();
        let Some(component) = components.iter_mut().find(|component| &component.id == id) else {
            return HostDecision::Refused(ActionRefusal::UnknownComponent(id.to_string()));
        };
        if component.required && !selected {
            return HostDecision::Refused(ActionRefusal::RequiredComponent(id.to_string()));
        }
        if component.selected == selected {
            return HostDecision::Acknowledged;
        }
        component.selected = selected;
        self.replan()
    }

    fn set_install_directory(&mut self, directory: Option<String>) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        if !self.snapshot.surface.allows_directory_override() {
            return HostDecision::Refused(ActionRefusal::DirectoryNotAllowed);
        }
        let directory = directory.filter(|directory| !directory.trim().is_empty());
        if self.snapshot.surface.install_directory() == directory.as_deref() {
            return HostDecision::Acknowledged;
        }
        self.snapshot.surface.set_install_directory(directory);
        self.replan()
    }

    fn operation_kind(&self, action: LifecycleAction) -> OperationKind {
        match action {
            LifecycleAction::Uninstall => OperationKind::Uninstall,
            LifecycleAction::Modify => OperationKind::Modify,
            LifecycleAction::Repair { .. } => OperationKind::Repair,
            _ => match &self.snapshot.surface {
                Surface::Install(options) if options.existing_version.is_some() => {
                    OperationKind::Upgrade
                }
                _ => OperationKind::Install,
            },
        }
    }

    fn start(&mut self, action: LifecycleAction, cleanup_lock: bool) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        let selection = Selection::from_surface(&self.snapshot.surface);
        self.retry = Some((action, selection.clone()));
        self.snapshot.operation = Some(self.operation_kind(action));
        self.begin();
        HostDecision::Run {
            action,
            selection,
            cleanup_lock,
        }
    }

    fn maintenance_op(&mut self, action: LifecycleAction, cleanup_lock: bool) -> HostDecision {
        if !self.capabilities.contains(Capability::Maintenance) {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        self.start(action, cleanup_lock)
    }

    fn update(&mut self) -> HostDecision {
        if !self.snapshot.surface.updates_enabled() {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        HostDecision::Update
    }

    fn request_uninstall(&mut self) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        if !self.capabilities.contains(Capability::Maintenance) {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        self.snapshot.state = InstallerState::ConfirmUninstall;
        HostDecision::Acknowledged
    }

    fn confirm_uninstall(&mut self) -> HostDecision {
        if self.snapshot.state != InstallerState::ConfirmUninstall {
            return HostDecision::Refused(ActionRefusal::NothingToConfirm);
        }
        self.start(LifecycleAction::Uninstall, true)
    }

    fn dismiss_uninstall(&mut self) -> HostDecision {
        if self.snapshot.state != InstallerState::ConfirmUninstall {
            return HostDecision::Refused(ActionRefusal::NothingToConfirm);
        }
        self.snapshot.state = self.resting_state();
        HostDecision::Acknowledged
    }

    fn cancel(&mut self) -> HostDecision {
        if self.snapshot.state == InstallerState::WaitingForSafeCancellation {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        if !self.running {
            return HostDecision::Refused(ActionRefusal::NothingToCancel);
        }
        self.snapshot.state = InstallerState::WaitingForSafeCancellation;
        HostDecision::Cancel
    }

    fn retry(&mut self) -> HostDecision {
        let Some((action, selection)) = self.retry.clone() else {
            return HostDecision::Refused(ActionRefusal::NothingToRetry);
        };
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        self.snapshot.operation = Some(self.operation_kind(action));
        self.begin();
        HostDecision::Run {
            action,
            selection,
            cleanup_lock: matches!(action, LifecycleAction::Uninstall),
        }
    }

    pub fn begin(&mut self) {
        self.running = true;
        self.snapshot.state = InstallerState::Running;
        self.snapshot.progress = Some(ProgressPresentation::new(
            OperationPhase::Prepare,
            0,
            0,
            "Preparing…",
        ));
        self.snapshot.diagnostic = None;
        self.snapshot.repair_drift.clear();
        self.snapshot.launch = None;
    }

    pub fn set_plan(&mut self, preview: zup_presentation::PlanPreview) {
        let preview = convert::plan(&preview);
        let selected: std::collections::BTreeSet<&ComponentId> = self
            .snapshot
            .surface
            .components()
            .iter()
            .filter(|component| component.selected)
            .map(|component| &component.id)
            .collect();
        let planned: std::collections::BTreeSet<&ComponentId> =
            preview.selected_components.iter().collect();
        let current = preview.scope == self.snapshot.surface.scope()
            && (self.snapshot.surface.components().is_empty() || planned == selected);
        if current {
            self.snapshot.plan = PlanStatus::Ready {
                preview: Box::new(preview),
            };
        }
    }

    pub fn plan_failed(&mut self, reason: String) {
        self.snapshot.plan = PlanStatus::Failed { reason };
    }

    pub fn set_log_path(&mut self, path: String) {
        self.log_path = Some(path);
    }

    pub fn set_update(&mut self, channel: Option<String>, state: UpdateState) {
        self.snapshot.update = Some(UpdatePresentation { channel, state });
    }

    pub fn set_repair_drift(&mut self, resources: Vec<String>) {
        if resources.is_empty() {
            return;
        }
        if let Surface::Maintenance(maintenance) = &mut self.snapshot.surface {
            maintenance.health = zup_preset_protocol::InstallationHealth::Drifted {
                resources: resources.clone(),
            };
        }
        self.snapshot.repair_drift = resources;
    }

    pub fn fail(&mut self, message: String, recovery_required: bool) {
        let diagnostic = diagnostic_for(&message, recovery_required);
        self.snapshot.diagnostic = Some(convert::diagnostic(&diagnostic));
        self.snapshot.state = if recovery_required {
            InstallerState::RecoveryRequired
        } else {
            InstallerState::Failed
        };
        self.finish();
    }

    pub fn observe(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::WaitingForAuthorization => self.advance("Waiting for approval…"),
            RuntimeEvent::WorkerConnected => self.advance("Starting…"),
            RuntimeEvent::PrerequisiteCheck {
                name, satisfied, ..
            } => {
                let label = if *satisfied {
                    format!("{name} is already installed")
                } else {
                    format!("{name} is needed")
                };
                self.advance(&label);
            }
            RuntimeEvent::PrerequisiteDownload {
                completed, total, ..
            } => {
                self.progress(
                    OperationPhase::Download,
                    *completed,
                    total.unwrap_or(0),
                    "Downloading a required component…",
                );
            }
            RuntimeEvent::PrerequisiteInstall { name, .. } => {
                self.advance(&format!("Installing {name}…"));
            }
            RuntimeEvent::RebootRequired { .. } => {
                self.fail("Restart the machine, then run setup again.".into(), false);
            }
            RuntimeEvent::PreflightStarted => self.advance("Checking for open applications…"),
            RuntimeEvent::ResourceBlocked { detail, .. } => {
                self.snapshot.state = InstallerState::Blocked {
                    blockers: detail.lines().map(str::to_owned).collect(),
                };
            }
            RuntimeEvent::StagingStarted { .. } => self.advance("Preparing files…"),
            RuntimeEvent::StagingProgress { detail, .. } => self.advance(detail),
            RuntimeEvent::OperationStarted { id } => self.advance(&operation_label(id)),
            RuntimeEvent::Progress {
                completed,
                total,
                action,
            } => {
                let phase = zup_presentation::OperationPhase::from_action(action);
                self.progress(convert::phase(phase), *completed, *total, action);
            }
            RuntimeEvent::RollingBack => self.advance("Restoring the previous state…"),
            RuntimeEvent::Completed { outcome } => match outcome.as_str() {
                "reboot_required" => {
                    self.snapshot.state = InstallerState::Failed;
                    self.finish();
                }
                "recovery_required" => {
                    self.snapshot.state = InstallerState::RecoveryRequired;
                    self.finish();
                }
                "cancelled" | "rolled_back" => {
                    self.snapshot.state = self.resting_state();
                    self.snapshot.diagnostic = None;
                    self.snapshot.progress = None;
                    self.finish();
                }
                _ => self.succeed(),
            },
            RuntimeEvent::Failed { kind, message } => {
                self.fail(message.clone(), kind == "recovery_required");
            }
            RuntimeEvent::LogPath { path } => self.set_log_path(path.clone()),
            RuntimeEvent::StateChanged { state } => self.observe_state(*state),
        }
    }

    fn observe_state(&mut self, state: zup_runtime::RuntimeState) {
        use zup_runtime::RuntimeState as S;
        match state {
            S::Preparing => self.advance("Preparing…"),
            S::CheckingPrerequisites => self.advance("Checking requirements…"),
            S::InstallingPrerequisites => self.advance("Installing required components…"),
            S::WaitingForAuthorization => self.advance("Waiting for approval…"),
            S::RebootRequired => {
                self.fail("Restart the machine, then run setup again.".into(), false);
            }
            S::ConnectingWorker => self.advance("Starting…"),
            S::Executing => self.advance("Installing files…"),
            S::RollingBack => self.advance("Restoring the previous state…"),
            S::Completed => self.succeed(),
            S::Cancelled => {
                self.snapshot.state = self.resting_state();
                self.finish();
            }
            S::Failed => {
                self.snapshot.state = InstallerState::Failed;
                self.finish();
            }
        }
    }

    pub fn finish_with(&mut self, outcome: &InstallOutcome) {
        match outcome {
            InstallOutcome::Committed => {
                self.retry = None;
                self.succeed();
            }
            InstallOutcome::RebootRequired {
                prerequisite_id,
                exit_code,
            } => self.fail(
                format!("Restart required by {prerequisite_id} ({exit_code})"),
                false,
            ),
            InstallOutcome::RolledBack | InstallOutcome::Cancelled => {
                self.snapshot.state = self.resting_state();
                self.snapshot.progress = None;
                self.snapshot.diagnostic = None;
                self.finish();
            }
            InstallOutcome::RecoveryRequired => self.fail("recovery required".into(), true),
            InstallOutcome::Busy { operation } => {
                self.snapshot.diagnostic = Some(DiagnosticPresentation {
                    kind: DiagnosticKind::Busy,
                    title: "Another operation is running".into(),
                    meaning: format!(
                        "This application is already being {operation} by another process."
                    ),
                    recovery: "Wait for it to finish, then try again.".into(),
                    technical_details: None,
                });
                self.snapshot.state = self.resting_state();
                self.snapshot.progress = None;
                self.finish();
            }
            InstallOutcome::Failed(message) => {
                if matches!(self.snapshot.state, InstallerState::Blocked { .. }) {
                    self.snapshot.progress = None;
                    self.finish();
                    return;
                }
                self.fail(message.clone(), false);
            }
        }
    }

    fn succeed(&mut self) {
        self.snapshot.state = InstallerState::Succeeded;
        self.snapshot.launch = self.launch_target();
        self.finish();
    }

    fn launch_target(&self) -> Option<LaunchTarget> {
        if !self.capabilities.contains(Capability::Launch)
            || self.snapshot.operation == Some(OperationKind::Uninstall)
        {
            return None;
        }
        let components = self.snapshot.surface.components();
        self.launchers
            .iter()
            .find(|launcher| {
                launcher.component.as_ref().is_none_or(|id| {
                    components
                        .iter()
                        .any(|component| &component.id == id && component.selected)
                })
            })
            .map(|launcher| launcher.target.clone())
    }

    fn finish(&mut self) {
        self.running = false;
        if !self.snapshot.state.is_active() {
            self.snapshot.progress = None;
        }
    }

    fn advance(&mut self, label: &str) {
        let waiting = self.snapshot.state == InstallerState::WaitingForSafeCancellation;
        let previous = self.snapshot.progress.take();
        let phase = convert::phase(zup_presentation::OperationPhase::from_action(label));
        self.snapshot.progress = Some(ProgressPresentation::new(
            phase,
            previous.as_ref().map_or(0, |progress| progress.completed),
            previous.as_ref().map_or(0, |progress| progress.total),
            label,
        ));
        if !waiting {
            self.snapshot.state = InstallerState::Running;
        }
    }

    fn progress(&mut self, phase: OperationPhase, completed: u64, total: u64, label: &str) {
        let waiting = self.snapshot.state == InstallerState::WaitingForSafeCancellation;
        self.snapshot.progress = Some(ProgressPresentation::new(phase, completed, total, label));
        if !waiting {
            self.snapshot.state = InstallerState::Running;
        }
    }
}

fn operation_label(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    if id.contains("service") {
        "Registering services…".into()
    } else if id.contains("launcher") {
        "Adding shortcuts…".into()
    } else if id.contains("file") {
        "Installing files…".into()
    } else {
        "Finishing…".into()
    }
}

fn diagnostic_for(
    message: &str,
    recovery_required: bool,
) -> zup_presentation::DiagnosticPresentation {
    if recovery_required {
        return zup_presentation::DiagnosticPresentation {
            kind: zup_presentation::DiagnosticKind::Recovery,
            title: "The last change didn't finish".into(),
            meaning: "Setup was interrupted while it was changing this computer, so the \
                      installation is between two versions."
                .into(),
            recovery: "Let setup finish restoring it before you make other changes.".into(),
            technical_details: Some(message.into()),
        };
    }
    zup_presentation::DiagnosticPresentation::from_message(message, recovery_required)
}
