//! The state a preset draws, and the only way it changes.

use zup_core::SelectedScope;
use zup_exec::LifecycleAction;
use zup_runtime::{InstallOutcome, RuntimeEvent};
use zup_ui_protocol::{
    ComponentId, DiagnosticKind, DiagnosticPresentation, InstallScope, MaintenanceState,
    OperationPhase, ProductIdentity, ProgressPresentation, UiAction, UiCapabilities, UiCapability,
    UiSnapshot, UiState, UiSurface, UpdatePresentation, UpdateState,
};

use crate::convert;

/// The choice an engine request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub scope: SelectedScope,
    pub components: Vec<zup_core::ComponentId>,
    pub install_directory: Option<String>,
}

impl Selection {
    /// The choices the surface currently shows, as an engine would read them.
    pub fn from_surface(surface: &UiSurface) -> Self {
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

/// What the host decided an action means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDecision {
    /// Apply a lifecycle with these choices.
    Run {
        action: LifecycleAction,
        selection: Selection,
        /// The uninstall ran from the copy the operating system is about to
        /// delete, so this process is the one that has to finish it.
        cleanup_lock: bool,
    },
    /// Ask the engine what this selection would change.
    Preview(Selection),
    /// Resolve the configured update channel.
    Update,
    /// Ask the running operation to stop at a safe boundary.
    Cancel,
    /// Reveal the session log.
    OpenLog,
    /// Put a diagnostic summary on the clipboard.
    CopyDiagnostics,
    /// The action was absorbed into the snapshot and needs no engine work.
    Acknowledged,
    /// The action does not apply here.
    Refused(ActionRefusal),
}

/// Why the host would not do what was asked.
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
}

/// The installer's authoritative view of itself.
///
/// A preset renders `snapshot()` and sends actions; nothing else about this
/// session's state is reachable from outside. Every mutation goes through here,
/// whether the caller is an engine reporting a real install or a development
/// environment pretending to be one, because a preset that could tell the
/// difference would be a preset developed against a machine that does not exist.
pub struct HostState {
    snapshot: UiSnapshot,
    capabilities: UiCapabilities,
    /// The lifecycle a `Retry` re-runs.
    retry: Option<(LifecycleAction, Selection)>,
    /// The session log, which a preset asks for rather than reads.
    log_path: Option<String>,
    /// Whether the last operation is still owned by a running thread.
    running: bool,
}

impl HostState {
    /// Open the install surface for a package that is not yet installed.
    pub fn install(
        product: ProductIdentity,
        options: zup_ui_protocol::InstallOptions,
        capabilities: UiCapabilities,
    ) -> Self {
        Self::open(product, UiSurface::Install(options), capabilities)
    }

    /// Open the maintenance surface for an installation that exists.
    pub fn maintenance(
        product: ProductIdentity,
        state: MaintenanceState,
        capabilities: UiCapabilities,
    ) -> Self {
        Self::open(product, UiSurface::Maintenance(state), capabilities)
    }

    fn open(product: ProductIdentity, surface: UiSurface, capabilities: UiCapabilities) -> Self {
        let state = match &surface {
            UiSurface::Install(_) => UiState::Options,
            UiSurface::Maintenance(_) => UiState::Maintenance,
        };
        Self {
            snapshot: UiSnapshot {
                product,
                surface,
                state,
                progress: None,
                plan: None,
                diagnostic: None,
                update: None,
                repair_drift: Vec::new(),
            },
            capabilities,
            retry: None,
            log_path: None,
            running: false,
        }
    }

    pub fn snapshot(&self) -> &UiSnapshot {
        &self.snapshot
    }

    pub fn capabilities(&self) -> &UiCapabilities {
        &self.capabilities
    }

    /// Where the engine is writing this session's log.
    ///
    /// Not on the snapshot: a path to a local file is a fact about this machine
    /// and not something a published preset can be shown.
    pub fn log_path(&self) -> Option<&str> {
        self.log_path.as_deref()
    }

    /// The state an operation returns to when it ends without committing.
    fn resting_state(&self) -> UiState {
        match &self.snapshot.surface {
            UiSurface::Install(_) => UiState::Options,
            UiSurface::Maintenance(_) => UiState::Maintenance,
        }
    }

    // -- Actions ----------------------------------------------------------

    /// Decide what an action means, and record what it changed.
    pub fn accept(&mut self, action: UiAction) -> HostDecision {
        match action {
            UiAction::SetScope { scope } => self.set_scope(scope),
            UiAction::SetComponent {
                component,
                selected,
            } => self.set_component(&component, selected),
            UiAction::SetInstallDirectory { directory } => {
                self.set_install_directory(Some(directory))
            }
            UiAction::Preview => self.preview(),
            UiAction::Install => self.start(LifecycleAction::Install, false),
            UiAction::Update => self.update(),
            UiAction::Modify => self.maintenance_op(LifecycleAction::Modify, false),
            UiAction::Repair => {
                self.maintenance_op(LifecycleAction::Repair { force_files: false }, false)
            }
            UiAction::RequestUninstall => self.request_uninstall(),
            UiAction::ConfirmUninstall => self.confirm_uninstall(),
            UiAction::DismissUninstall => self.dismiss_uninstall(),
            UiAction::Cancel => self.cancel(),
            UiAction::Retry => self.retry(),
            UiAction::OpenLog => {
                if self.log_path.is_some() {
                    HostDecision::OpenLog
                } else {
                    HostDecision::Refused(ActionRefusal::NoLog)
                }
            }
            UiAction::CopyDiagnostics => HostDecision::CopyDiagnostics,
            UiAction::Close => HostDecision::Acknowledged,
        }
    }

    fn set_scope(&mut self, scope: InstallScope) -> HostDecision {
        if !self.snapshot.surface.scopes().contains(&scope) {
            return HostDecision::Refused(ActionRefusal::UnsupportedScope);
        }
        self.snapshot.surface.set_scope(scope);
        HostDecision::Acknowledged
    }

    fn set_component(&mut self, id: &ComponentId, selected: bool) -> HostDecision {
        let components = self.snapshot.surface.components_mut();
        let Some(component) = components.iter_mut().find(|component| &component.id == id) else {
            return HostDecision::Refused(ActionRefusal::UnknownComponent(id.to_string()));
        };
        if component.required && !selected {
            return HostDecision::Refused(ActionRefusal::RequiredComponent(id.to_string()));
        }
        component.selected = selected;
        HostDecision::Acknowledged
    }

    fn set_install_directory(&mut self, directory: Option<String>) -> HostDecision {
        if directory.is_some() && !self.snapshot.surface.allows_directory_override() {
            return HostDecision::Refused(ActionRefusal::DirectoryNotAllowed);
        }
        self.snapshot.surface.set_install_directory(directory);
        HostDecision::Acknowledged
    }

    fn preview(&mut self) -> HostDecision {
        if !self.capabilities.contains(UiCapability::PlanPreview) {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        HostDecision::Preview(Selection::from_surface(&self.snapshot.surface))
    }

    /// Start a lifecycle, recording the intent a `Retry` would repeat.
    fn start(&mut self, action: LifecycleAction, cleanup_lock: bool) -> HostDecision {
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        let selection = Selection::from_surface(&self.snapshot.surface);
        self.retry = Some((action, selection.clone()));
        self.begin();
        HostDecision::Run {
            action,
            selection,
            cleanup_lock,
        }
    }

    /// A lifecycle that only an installation which already exists can run.
    fn maintenance_op(&mut self, action: LifecycleAction, cleanup_lock: bool) -> HostDecision {
        if !self.capabilities.contains(UiCapability::Maintenance) {
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
        if !self.capabilities.contains(UiCapability::Maintenance) {
            return HostDecision::Refused(ActionRefusal::UnsupportedSurface);
        }
        self.snapshot.state = UiState::ConfirmUninstall;
        HostDecision::Acknowledged
    }

    fn confirm_uninstall(&mut self) -> HostDecision {
        if self.snapshot.state != UiState::ConfirmUninstall {
            return HostDecision::Refused(ActionRefusal::NothingToConfirm);
        }
        self.start(LifecycleAction::Uninstall, true)
    }

    fn dismiss_uninstall(&mut self) -> HostDecision {
        if self.snapshot.state != UiState::ConfirmUninstall {
            return HostDecision::Refused(ActionRefusal::NothingToConfirm);
        }
        self.snapshot.state = self.resting_state();
        HostDecision::Acknowledged
    }

    fn cancel(&mut self) -> HostDecision {
        if self.snapshot.state == UiState::WaitingForSafeCancellation {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        if !self.running {
            return HostDecision::Refused(ActionRefusal::NothingToCancel);
        }
        self.snapshot.state = UiState::WaitingForSafeCancellation;
        HostDecision::Cancel
    }

    fn retry(&mut self) -> HostDecision {
        let Some((action, selection)) = self.retry.clone() else {
            return HostDecision::Refused(ActionRefusal::NothingToRetry);
        };
        if self.running {
            return HostDecision::Refused(ActionRefusal::Busy);
        }
        self.begin();
        HostDecision::Run {
            action,
            selection,
            cleanup_lock: matches!(action, LifecycleAction::Uninstall),
        }
    }

    // -- Engine events ----------------------------------------------------

    /// An operation has started. The host owns the state from here.
    pub fn begin(&mut self) {
        self.running = true;
        self.snapshot.state = UiState::Running;
        self.snapshot.progress = Some(ProgressPresentation::new(
            OperationPhase::Prepare,
            0,
            0,
            "Preparing…",
        ));
        self.snapshot.diagnostic = None;
    }

    /// Record the answer to "what will change".
    pub fn set_plan(&mut self, preview: zup_presentation::PlanPreview) {
        let preview = convert::plan(&preview);
        self.snapshot
            .surface
            .set_install_directory(Some(preview.install_directory.clone()));
        self.snapshot.plan = Some(preview);
    }

    /// Record where the engine is writing its log.
    pub fn set_log_path(&mut self, path: String) {
        self.log_path = Some(path);
    }

    /// Record the update check's result.
    pub fn set_update(&mut self, channel: Option<String>, state: UpdateState) {
        self.snapshot.update = Some(UpdatePresentation { channel, state });
    }

    /// Record the resources a repair left alone, because they had drifted.
    pub fn set_repair_drift(&mut self, resources: Vec<String>) {
        if resources.is_empty() {
            return;
        }
        if let UiSurface::Maintenance(maintenance) = &mut self.snapshot.surface {
            maintenance.health = zup_ui_protocol::InstallationHealth::Drifted {
                resources: resources.clone(),
            };
        }
        self.snapshot.repair_drift = resources;
    }

    /// Record a failure the engine reported before it started a transaction.
    pub fn fail(&mut self, message: String, recovery_required: bool) {
        let diagnostic = diagnostic_for(&message, recovery_required);
        self.snapshot.diagnostic = Some(convert::diagnostic(&diagnostic));
        self.snapshot.state = if recovery_required {
            UiState::RecoveryRequired
        } else {
            UiState::Failed
        };
        self.finish();
    }

    /// Translate one engine event into presentation state.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::WaitingForAuthorization => self.advance("Waiting for approval…"),
            RuntimeEvent::WorkerConnected => self.advance("Starting…"),
            RuntimeEvent::PrerequisiteCheck {
                name, satisfied, ..
            } => {
                let label = if *satisfied {
                    format!("✓ {name}")
                } else {
                    format!("↓ {name}")
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
                    "Downloading required component…",
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
                self.snapshot.state = UiState::Blocked {
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
            RuntimeEvent::Completed { outcome } => {
                self.snapshot.state = match outcome.as_str() {
                    "reboot_required" => UiState::Failed,
                    "recovery_required" => UiState::RecoveryRequired,
                    _ => UiState::Succeeded,
                };
                self.finish();
            }
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
            S::Completed => {
                self.snapshot.state = UiState::Succeeded;
                self.finish();
            }
            S::Cancelled => {
                self.snapshot.state = self.resting_state();
                self.finish();
            }
            S::Failed => {
                self.snapshot.state = UiState::Failed;
                self.finish();
            }
        }
    }

    /// Translate the outcome of a finished transaction.
    pub fn finish_with(&mut self, outcome: &InstallOutcome) {
        match outcome {
            InstallOutcome::Committed => {
                self.snapshot.state = UiState::Succeeded;
                self.snapshot.progress = None;
                self.retry = None;
                self.finish();
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
            // Not a failure: the installation is fine, another operation holds
            // it, and the right answer is the surface it started from with an
            // explanation rather than a red dialog.
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
                // A blocked preflight already produced a `Blocked` state with
                // the processes that hold the files. Reporting it again as a
                // failure would throw away the only actionable part of it.
                if matches!(self.snapshot.state, UiState::Blocked { .. }) {
                    self.snapshot.progress = None;
                    self.finish();
                    return;
                }
                self.fail(message.clone(), false);
            }
        }
    }

    /// The operation is no longer owned by a thread.
    fn finish(&mut self) {
        self.running = false;
        if !self.snapshot.state.is_active() {
            self.snapshot.progress = None;
        }
    }

    /// Report a new position in the run without disturbing the counters.
    fn advance(&mut self, label: &str) {
        let waiting = self.snapshot.state == UiState::WaitingForSafeCancellation;
        let previous = self.snapshot.progress.take();
        let phase = convert::phase(zup_presentation::OperationPhase::from_action(label));
        self.snapshot.progress = Some(ProgressPresentation::new(
            phase,
            previous.as_ref().map_or(0, |progress| progress.completed),
            previous.as_ref().map_or(0, |progress| progress.total),
            label,
        ));
        if !waiting {
            self.snapshot.state = UiState::Running;
        }
    }

    fn progress(&mut self, phase: OperationPhase, completed: u64, total: u64, label: &str) {
        let waiting = self.snapshot.state == UiState::WaitingForSafeCancellation;
        self.snapshot.progress = Some(ProgressPresentation::new(phase, completed, total, label));
        if !waiting {
            self.snapshot.state = UiState::Running;
        }
    }
}

/// What the engine calls a node, as a person reads it.
fn operation_label(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    if id.contains("service") {
        "Registering services…".into()
    } else if id.contains("launcher") {
        "Updating launchers…".into()
    } else if id.contains("file") {
        "Installing files…".into()
    } else {
        "Finishing…".into()
    }
}

/// The explanation a failure gets, in the shape a person can act on.
fn diagnostic_for(
    message: &str,
    recovery_required: bool,
) -> zup_presentation::DiagnosticPresentation {
    if recovery_required {
        return zup_presentation::DiagnosticPresentation {
            kind: zup_presentation::DiagnosticKind::Recovery,
            title: "Recovery is required".into(),
            meaning: "The last transaction did not finish safely.".into(),
            recovery: "Run recovery before starting another operation.".into(),
            technical_details: Some(message.into()),
        };
    }
    zup_presentation::DiagnosticPresentation::from_message(message, recovery_required)
}
