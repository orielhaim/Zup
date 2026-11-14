use zup_core::SelectedScope;
use zup_plan::LifecycleAction;
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
            install_directory: surface
                .allows_directory_override()
                .then(|| surface.install_directory().map(str::to_owned))
                .flatten(),
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
    Recover {
        transaction: zup_core::TransactionId,
    },
    Run {
        action: LifecycleAction,
        selection: Selection,
    },
    Plan(Selection),
    Update(zup_runtime::UpdateRequest),
    Cancel,
    Refresh,
    OpenLog,
    CopyDiagnostics,
    Launch(LaunchTarget),
    Acknowledged,
    Refused(ActionRefusal),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionRefusal {
    #[error("recover the pending transaction before requesting another operation")]
    RecoveryRequired,
    #[error("there is no known transaction requiring recovery")]
    NoRecovery,
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
    #[error("saved deployment {0} is not available in this installation")]
    RetainedDeploymentNotFound(zup_core::ActivationRevision),
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
    pub fn refresh_installation(&mut self, fresh: Self) {
        self.snapshot.product = fresh.snapshot.product;
        self.snapshot.surface = fresh.snapshot.surface;
        self.snapshot.plan = fresh.snapshot.plan;
        self.capabilities = fresh.capabilities;
        self.launchers = fresh.launchers;
    }

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
                recovery: None,
                settlement: None,
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
        if self.snapshot.state == InstallerState::RecoveryRequired
            && !matches!(
                action,
                Action::Recover { .. }
                    | Action::OpenLog
                    | Action::CopyDiagnostics
                    | Action::Close
                    | Action::Retry
                    | Action::Launch
            )
        {
            return HostDecision::Refused(ActionRefusal::RecoveryRequired);
        }
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
            Action::Install => self.start(LifecycleAction::Install),
            Action::Update => self.update(),
            Action::Modify => self.maintenance_op(LifecycleAction::Modify),
            Action::Downgrade => self.maintenance_op(LifecycleAction::Downgrade),
            Action::Cleanup { keep } => self.maintenance_op(LifecycleAction::Cleanup { keep }),
            Action::Rollback { revision } => {
                self.maintenance_op(LifecycleAction::Rollback { revision })
            }
            Action::Repair => self.maintenance_op(LifecycleAction::Repair { force: false }),
            Action::Recover { transaction } => {
                if self.running {
                    HostDecision::Refused(ActionRefusal::Busy)
                } else if self
                    .snapshot
                    .recovery
                    .as_ref()
                    .map(|target| target.transaction)
                    != Some(transaction)
                {
                    HostDecision::Refused(ActionRefusal::NoRecovery)
                } else {
                    self.snapshot.operation = Some(OperationKind::Recover);
                    self.begin();
                    HostDecision::Recover { transaction }
                }
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
        if self.snapshot.recovery.is_some()
            || self.snapshot.state == InstallerState::RefreshRequired
        {
            return None;
        }
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
            LifecycleAction::Downgrade => OperationKind::Downgrade,
            LifecycleAction::Rollback { .. } => OperationKind::Rollback,
            LifecycleAction::Cleanup { .. } => OperationKind::Cleanup,
            LifecycleAction::Repair { .. } => OperationKind::Repair,
            _ => match &self.snapshot.surface {
                Surface::Install(options) if options.existing_version.is_some() => {
                    OperationKind::Upgrade
                }
                _ => OperationKind::Install,
            },
        }
    }

    fn start(&mut self, action: LifecycleAction) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        let selection = Selection::from_surface(&self.snapshot.surface);
        self.retry = Some((action, selection.clone()));
        self.snapshot.operation = Some(self.operation_kind(action));
        self.begin();
        HostDecision::Run { action, selection }
    }

    fn maintenance_op(&mut self, action: LifecycleAction) -> HostDecision {
        if !self.capabilities.contains(Capability::Maintenance) {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        if let LifecycleAction::Rollback { revision } = action
            && !matches!(&self.snapshot.surface, Surface::Maintenance(state) if state.retained.iter().any(|snapshot| snapshot.revision == revision))
        {
            return HostDecision::Refused(ActionRefusal::RetainedDeploymentNotFound(revision));
        }
        self.start(action)
    }

    fn update(&mut self) -> HostDecision {
        if !self.snapshot.surface.updates_enabled() {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        let request = match self.snapshot.update.as_ref().map(|update| &update.state) {
            Some(UpdateState::Available { candidate, .. }) => zup_runtime::UpdateRequest::Install {
                expected: Some(candidate.clone()),
            },
            _ => zup_runtime::UpdateRequest::Check,
        };
        self.begin();
        HostDecision::Update(request)
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
        self.start(LifecycleAction::Uninstall)
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
        if self.snapshot.state == InstallerState::RefreshRequired {
            return HostDecision::Refresh;
        }
        if let Some(target) = &self.snapshot.recovery {
            return self.accept(Action::Recover {
                transaction: target.transaction,
            });
        }
        let Some((action, selection)) = self.retry.clone() else {
            return HostDecision::Refused(ActionRefusal::NothingToRetry);
        };
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        self.snapshot.operation = Some(self.operation_kind(action));
        self.begin();
        HostDecision::Run { action, selection }
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

    pub fn fail(&mut self, message: String, outcome: zup_presentation::ProcessOutcome) {
        let diagnostic = zup_presentation::DiagnosticPresentation::for_outcome(outcome, &message);
        self.snapshot.diagnostic = Some(convert::diagnostic(&diagnostic));
        self.snapshot.state = InstallerState::Failed;
        if outcome == zup_presentation::ProcessOutcome::RecoveryRequired {
            self.snapshot.state = InstallerState::RefreshRequired;
        }
        self.finish();
    }

    pub fn set_recovery(&mut self, target: zup_core::RecoverySummary) {
        if target.scope != convert::engine_scope(self.snapshot.surface.scope()) {
            self.refresh_required("The protected transaction belongs to a different installation scope. Refresh deployment state.".into());
            return;
        }
        let detail = format!(
            "{} {}\nApplication: {}\nScope: {}\nPlatform: {}\nInstallation: {}\nDirectory: {}\nRecovery: {:?}\nTransaction: {}",
            target.version,
            target.detail,
            target.app,
            target.scope,
            target.target,
            target.installation,
            target
                .directory
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "No active deployment directory".into()),
            target.kind,
            target.transaction
        );
        self.snapshot.settlement = match target.kind {
            zup_core::RecoveryKind::CommittedCleanup
            | zup_core::RecoveryKind::CommittedReconciliation => {
                Some(zup_core::DeploymentSettlement::Committed)
            }
            _ => None,
        };
        let locations = target
            .cleanup
            .iter()
            .map(|cleanup| match cleanup {
                zup_core::RecoveryCleanup::Generation { directory } => {
                    format!("Retired generation: {directory}")
                }
                zup_core::RecoveryCleanup::FixedFile { path } => {
                    format!("Unreferenced file: {path}")
                }
                zup_core::RecoveryCleanup::Metadata { transaction } => {
                    format!("Unreferenced transaction metadata: {transaction}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.fail(
            format!("{detail}\n{locations}"),
            zup_presentation::ProcessOutcome::RecoveryRequired,
        );
        if let Some(diagnostic) = &mut self.snapshot.diagnostic {
            let (title, meaning, recovery) = match target.kind {
                zup_core::RecoveryKind::CommittedCleanup => (
                    "Deployment committed; cleanup pending",
                    "The deployment is committed. Remaining native cleanup is incomplete.",
                    "Close applications using retired files, then choose Recover. Recovery resumes only the remaining cleanup.",
                ),
                zup_core::RecoveryKind::CommittedReconciliation => (
                    "Deployment committed; reconciliation pending",
                    "The deployment is committed. Its protected ownership metadata needs reconciliation.",
                    "Choose Recover to reconcile the original protected transaction.",
                ),
                zup_core::RecoveryKind::Interrupted => (
                    "Interrupted deployment",
                    "A protected transaction has unfinished native effects.",
                    "Choose Recover to reconcile the recorded effects before another change.",
                ),
                zup_core::RecoveryKind::PublicationUncertain => (
                    "Deployment publication needs verification",
                    "The journal cannot yet confirm whether publication completed.",
                    "Choose Recover to verify protected publication evidence. Setup will preserve any committed deployment.",
                ),
                zup_core::RecoveryKind::OperatorRequired => (
                    "Recovery needs an operator",
                    "Automatic rollback cannot safely restore the previous deployment.",
                    "Use the recovery assessment and explicit decision commands shown in the details. The GUI cannot approve a forward decision.",
                ),
            };
            diagnostic.title = title.into();
            diagnostic.meaning = format!("{} {}: {meaning}", target.app, target.version);
            diagnostic.recovery = recovery.into();
        }
        self.snapshot.product.version = target.version.to_string();
        self.snapshot.plan = PlanStatus::Unsupported;
        self.snapshot.recovery = Some(target);
        self.snapshot.state = InstallerState::RecoveryRequired;
        self.snapshot.launch = self.launch_target().filter(|_| {
            self.snapshot.settlement == Some(zup_core::DeploymentSettlement::Committed)
        });
    }

    pub fn refresh_required(&mut self, detail: String) {
        let meaning = match self.snapshot.settlement {
            Some(zup_core::DeploymentSettlement::Committed) => {
                "The deployment committed. Setup needs to refresh its state or finish housekeeping."
            }
            Some(zup_core::DeploymentSettlement::RolledBack) => {
                "Recovery restored the previous deployment. Setup needs to refresh its state."
            }
            None => "Setup could not inspect the protected deployment state.",
        };
        self.snapshot.diagnostic = Some(DiagnosticPresentation {
            kind: DiagnosticKind::Unknown,
            title: "Refresh deployment state".into(),
            meaning: meaning.into(),
            recovery: "Refresh to inspect the protected journal and retry housekeeping. If unavailable, close setup and reopen a trusted maintenance carrier.".into(),
            technical_details: Some(detail),
        });
        self.snapshot.recovery = None;
        self.snapshot.state = InstallerState::RefreshRequired;
        self.snapshot.launch = self.launch_target().filter(|_| {
            self.snapshot.settlement == Some(zup_core::DeploymentSettlement::Committed)
        });
        self.finish();
    }

    pub fn observe(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::PlanPrepared { plan } => {
                match zup_presentation::PlanPreview::from_transaction_plan(
                    plan,
                    plan.requirements.scope,
                    None,
                ) {
                    Ok(preview) => self.set_plan(preview),
                    Err(error) => self.plan_failed(error.to_string()),
                }
            }
            RuntimeEvent::WaitingForAuthorization => {
                self.advance(OperationPhase::Prepare, "Waiting for approval…")
            }
            RuntimeEvent::WorkerConnected => self.advance(OperationPhase::Prepare, "Starting…"),
            RuntimeEvent::PrerequisiteCheck {
                name, satisfied, ..
            } => {
                let label = if *satisfied {
                    format!("{name} is already installed")
                } else {
                    format!("{name} is needed")
                };
                self.advance(OperationPhase::Prepare, &label);
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
                self.advance(OperationPhase::System, &format!("Installing {name}…"));
            }
            RuntimeEvent::RebootRequired { .. } => {
                self.snapshot.state = InstallerState::RebootRequired;
                self.finish();
            }
            RuntimeEvent::PreflightStarted => {
                self.advance(OperationPhase::Prepare, "Checking for open applications…")
            }
            RuntimeEvent::ResourceBlocked { detail, .. } => {
                self.snapshot.state = InstallerState::Blocked {
                    blockers: detail.lines().map(str::to_owned).collect(),
                };
            }
            RuntimeEvent::StagingStarted { .. } => {
                self.advance(OperationPhase::Prepare, "Preparing files…")
            }
            RuntimeEvent::StagingProgress { detail, .. } => {
                self.advance(OperationPhase::Prepare, detail)
            }
            RuntimeEvent::OperationStarted { .. } => {
                self.advance(OperationPhase::Prepare, "Applying changes…")
            }
            RuntimeEvent::Progress {
                completed,
                total,
                action,
                phase,
                ..
            } => {
                self.progress(*phase, *completed, *total, action);
            }
            RuntimeEvent::RollingBack => {
                self.advance(OperationPhase::System, "Restoring the previous state…")
            }
            RuntimeEvent::Completed { outcome } => match outcome {
                zup_runtime::CompletionOutcome::Committed
                | zup_runtime::CompletionOutcome::Recovered { .. } => self.succeed(),
                zup_runtime::CompletionOutcome::RebootRequired => {
                    self.snapshot.state = InstallerState::RebootRequired;
                    self.finish();
                }
                zup_runtime::CompletionOutcome::Cancelled
                | zup_runtime::CompletionOutcome::RolledBack => {
                    self.snapshot.state = self.resting_state();
                    self.snapshot.diagnostic = None;
                    self.snapshot.progress = None;
                    self.finish();
                }
            },
            RuntimeEvent::Failed { kind, message } => {
                self.fail(message.clone(), (*kind).into());
            }
            RuntimeEvent::LogPath { path } => self.set_log_path(path.clone()),
            RuntimeEvent::StateChanged { state } => self.observe_state(*state),
        }
    }

    fn observe_state(&mut self, state: zup_runtime::RuntimeState) {
        use zup_runtime::RuntimeState as S;
        match state {
            S::Preparing => self.advance(OperationPhase::Prepare, "Preparing…"),
            S::CheckingPrerequisites => {
                self.advance(OperationPhase::Prepare, "Checking requirements…")
            }
            S::InstallingPrerequisites => {
                self.advance(OperationPhase::Prepare, "Installing required components…")
            }
            S::WaitingForAuthorization => {
                self.advance(OperationPhase::Prepare, "Waiting for approval…")
            }
            S::RebootRequired => {
                self.snapshot.state = InstallerState::RebootRequired;
                self.finish();
            }
            S::ConnectingWorker => self.advance(OperationPhase::Prepare, "Starting…"),
            S::Executing => self.advance(OperationPhase::Files, "Installing files…"),
            S::RollingBack => self.advance(OperationPhase::System, "Restoring the previous state…"),
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
        self.snapshot.settlement = match outcome {
            InstallOutcome::Committed
            | InstallOutcome::CommittedWithWarning { .. }
            | InstallOutcome::CommittedPendingReconciliation { .. }
            | InstallOutcome::Recovered {
                settlement: zup_runtime::RecoverySettlement::Committed,
            } => Some(zup_core::DeploymentSettlement::Committed),
            InstallOutcome::RolledBack
            | InstallOutcome::Recovered {
                settlement: zup_runtime::RecoverySettlement::RolledBack,
            } => Some(zup_core::DeploymentSettlement::RolledBack),
            _ => self.snapshot.settlement,
        };
        match outcome {
            InstallOutcome::RecoveryInspected { target, .. } => {
                if let Some(target) = target { self.set_recovery(target.clone()); }
                else { self.snapshot.recovery = None; self.snapshot.state = self.resting_state();  self.finish(); }
            }
            InstallOutcome::RecoveryAssessed { assessment } => self.refresh_required(
                format!("Assessment complete; deployment remains blocked. {} Use recover --transaction {} --assess --output json to review evidence and --decision FILE for an admissible forward decision.",
                    assessment.blockers.iter().map(|blocker| blocker.detail.as_str()).collect::<Vec<_>>().join(" "), assessment.transaction)),
            InstallOutcome::Committed | InstallOutcome::Recovered { .. } => {
                self.snapshot.recovery = None;
                        self.retry = None;
                self.succeed();
            }
            InstallOutcome::RebootRequired {
                prerequisite_id,
                exit_code,
            } => {
                self.snapshot.state = InstallerState::RebootRequired;
                self.snapshot.diagnostic = Some(convert::diagnostic(
                    &zup_presentation::DiagnosticPresentation::for_outcome(
                        zup_presentation::ProcessOutcome::RebootRequired,
                        &format!("Restart required by {prerequisite_id} ({exit_code})"),
                    ),
                ));
                self.finish();
            }
            InstallOutcome::RolledBack | InstallOutcome::Cancelled => {
                self.snapshot.state = self.resting_state();
                self.snapshot.progress = None;
                self.snapshot.diagnostic = None;
                self.finish();
            }
            InstallOutcome::RecoveryRequired { detail, .. }
            | InstallOutcome::CommittedPendingReconciliation { detail, .. }
            | InstallOutcome::CommittedWithWarning { detail } => self.refresh_required(detail.clone()),
            InstallOutcome::AuthorizationRequired => {
                self.fail(
                    "authorization required".into(),
                    zup_presentation::ProcessOutcome::AuthorizationRequired,
                );
            }
            InstallOutcome::AuthorizationCancelled => {
                self.fail(
                    "authorization cancelled".into(),
                    zup_presentation::ProcessOutcome::AuthorizationRequired,
                );
            }
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
                self.fail(message.clone(), zup_presentation::ProcessOutcome::Failure);
            }
        }
    }

    pub fn finish_update_check(&mut self) {
        self.snapshot.state = self.resting_state();
        self.snapshot.diagnostic = None;
        self.finish();
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

    fn advance(&mut self, phase: OperationPhase, label: &str) {
        let waiting = self.snapshot.state == InstallerState::WaitingForSafeCancellation;
        let previous = self.snapshot.progress.take();
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
