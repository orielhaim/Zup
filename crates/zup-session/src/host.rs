use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::{AppId, ComponentId, InstallScope, Installer, SelectedScope, TargetBuildPlan};
use zup_exec::InstallLedger;
use zup_preset_protocol::UpdateState;
use zup_runtime::{InstallOutcome, OperationHost, PreparedOperation, RuntimeBackend, RuntimeEvent};

pub use zup_preset_host::{HostDecision, HostState, Selection};

#[derive(Debug, Clone, Default)]
pub struct SessionLaunch {
    pub scope: Option<SelectedScope>,
    pub state_root: Option<PathBuf>,
    pub preselected: Option<Vec<ComponentId>>,
    pub maintenance: bool,
    pub auto_uninstall: bool,
    pub interactive: bool,
}

#[derive(Debug, Clone)]
pub struct ScopedInstallation {
    pub scope: SelectedScope,
    pub state_root: PathBuf,
    pub ledger: Option<InstallLedger>,
}

#[derive(Debug, Clone)]
pub struct OperationContext {
    pub scope: SelectedScope,
    pub state_root: PathBuf,
    pub ledger: Option<InstallLedger>,
}

#[derive(Debug, Clone)]
pub struct SessionDiscovery {
    pub build: TargetBuildPlan,
    pub installations: Vec<ScopedInstallation>,
}

impl SessionDiscovery {
    pub fn installer(&self) -> &Installer {
        &self.build.installer
    }

    pub fn app_id(&self) -> &AppId {
        &self.build.installer.app.id
    }

    pub fn installation(&self, scope: SelectedScope) -> Option<&ScopedInstallation> {
        self.installations
            .iter()
            .find(|installation| installation.scope == scope)
    }

    pub fn context(&self, scope: SelectedScope) -> Result<OperationContext, String> {
        let installation = self
            .installation(scope)
            .ok_or_else(|| format!("this installer has no {scope} installation to operate on"))?;
        Ok(OperationContext {
            scope: installation.scope,
            state_root: installation.state_root.clone(),
            ledger: installation.ledger.clone(),
        })
    }
}

pub fn select_scope(
    declared: InstallScope,
    launch: &SessionLaunch,
    installations: &[ScopedInstallation],
) -> Result<SelectedScope, String> {
    if !launch.interactive {
        return declared
            .select(launch.scope)
            .map_err(|error| error.to_string());
    }
    if let Some(scope) = launch.scope {
        return declared
            .select(Some(scope))
            .map_err(|error| error.to_string());
    }
    let permitted = declared.scopes();
    let installed: Vec<SelectedScope> = permitted
        .iter()
        .copied()
        .filter(|scope| {
            installations
                .iter()
                .any(|installation| installation.scope == *scope && installation.ledger.is_some())
        })
        .collect();
    match installed.as_slice() {
        [scope, ..] => Ok(*scope),
        [] => permitted
            .first()
            .copied()
            .ok_or_else(|| "this installer declares no installation scope".to_owned()),
    }
}

pub struct PresetLaunch {
    pub executable: PathBuf,
    pub configuration: zup_preset_protocol::Configuration,
    pub capabilities: zup_preset_protocol::Capabilities,
    pub product: zup_preset_protocol::ProductIdentity,
    pub environment: zup_preset_protocol::HostEnvironment,
    pub stage: Option<zup_preset_host::PresetStage>,
}

pub trait SessionHost {
    type Bound<'a>: OperationHost
    where
        Self: 'a;

    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String>;

    fn bind<'a>(&'a self, context: &'a OperationContext) -> Result<Self::Bound<'a>, String>;

    fn backend(
        &self,
        context: &OperationContext,
        prepared: &PreparedOperation,
    ) -> Result<Arc<dyn RuntimeBackend>, String>;

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
        context: &OperationContext,
    ) -> Result<PresetLaunch, String>;

    fn after_commit(
        &self,
        _context: &OperationContext,
        _prepared: &PreparedOperation,
    ) -> Result<(), String> {
        Ok(())
    }

    fn update(
        &self,
        _scope: SelectedScope,
        _status: &mut dyn FnMut(UpdateState),
    ) -> Result<(), String> {
        Err("updates are not supported here".into())
    }

    fn open_log(&self, path: &str) -> Result<(), String> {
        opener::open(path).map_err(|error| error.to_string())
    }

    fn copy_diagnostics(&self, _text: &str) {}
}

pub fn initial_state(
    discovery: &SessionDiscovery,
    context: &OperationContext,
    preselected: Option<&[ComponentId]>,
) -> HostState {
    let installer = discovery.installer();
    let launchers = zup_preset_host::surface::launchers(installer);
    let state = match context.ledger.as_ref() {
        Some(ledger) => {
            let maintenance =
                zup_preset_host::surface::maintenance_state(installer, ledger, context.scope);
            HostState::maintenance(
                zup_preset_host::surface::product(installer),
                maintenance,
                zup_preset_host::surface::capabilities(installer, true),
            )
        }
        None => {
            let options = zup_preset_host::surface::install_options(
                installer,
                context.scope,
                None,
                preselected,
                None,
            );
            HostState::install(
                zup_preset_host::surface::product(installer),
                options,
                zup_preset_host::surface::capabilities(installer, false),
            )
        }
    };
    state.with_launchers(launchers)
}

pub fn selection_of(installer: &Installer, selection: &Selection) -> (Vec<String>, Vec<String>) {
    let enabled = selection
        .components
        .iter()
        .map(ToString::to_string)
        .collect();
    let disabled = installer
        .components
        .iter()
        .filter(|component| !component.required && !selection.components.contains(&component.id))
        .map(|component| component.id.to_string())
        .collect();
    (enabled, disabled)
}

pub fn is_terminal(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::Completed { .. } | RuntimeEvent::Failed { .. }
    )
}

pub fn process_outcome(outcome: &InstallOutcome) -> zup_presentation::ProcessOutcome {
    match outcome {
        InstallOutcome::RebootRequired { .. } => zup_presentation::ProcessOutcome::RebootRequired,
        InstallOutcome::Committed => zup_presentation::ProcessOutcome::Success,
        InstallOutcome::Cancelled => zup_presentation::ProcessOutcome::Cancelled,
        InstallOutcome::RecoveryRequired
        | InstallOutcome::CommittedPendingReconciliation { .. } => {
            zup_presentation::ProcessOutcome::RecoveryRequired
        }
        InstallOutcome::AuthorizationRequired | InstallOutcome::AuthorizationCancelled => {
            zup_presentation::ProcessOutcome::AuthorizationRequired
        }
        InstallOutcome::Busy { .. } => zup_presentation::ProcessOutcome::InstallationBusy,
        InstallOutcome::RolledBack => zup_presentation::ProcessOutcome::Failure,
        InstallOutcome::Failed(message) => zup_presentation::ProcessOutcome::from_message(message),
    }
}

pub fn state_name(state: zup_runtime::RuntimeState) -> &'static str {
    match state {
        zup_runtime::RuntimeState::Preparing => "preparing",
        zup_runtime::RuntimeState::CheckingPrerequisites => "checking_prerequisites",
        zup_runtime::RuntimeState::InstallingPrerequisites => "installing_prerequisites",
        zup_runtime::RuntimeState::RebootRequired => "reboot_required",
        zup_runtime::RuntimeState::WaitingForAuthorization => "waiting_for_authorization",
        zup_runtime::RuntimeState::ConnectingWorker => "connecting_worker",
        zup_runtime::RuntimeState::Executing => "executing",
        zup_runtime::RuntimeState::RollingBack => "rolling_back",
        zup_runtime::RuntimeState::Completed => "completed",
        zup_runtime::RuntimeState::Cancelled => "cancelling",
        zup_runtime::RuntimeState::Failed => "failed",
    }
}

pub fn automation_events(event: &RuntimeEvent) -> Vec<zup_presentation::InstallerEvent> {
    use zup_presentation::InstallerEvent;
    match event {
        RuntimeEvent::StateChanged { state } => {
            if *state == zup_runtime::RuntimeState::Cancelled {
                vec![InstallerEvent::Cancelling {
                    state: "safe_boundary".into(),
                }]
            } else {
                vec![InstallerEvent::Phase {
                    state: state_name(*state).into(),
                }]
            }
        }
        RuntimeEvent::WaitingForAuthorization => vec![InstallerEvent::Phase {
            state: "waiting_for_authorization".into(),
        }],
        RuntimeEvent::WorkerConnected => vec![InstallerEvent::Phase {
            state: "worker_connected".into(),
        }],
        RuntimeEvent::PreflightStarted => vec![InstallerEvent::Phase {
            state: "preflight".into(),
        }],
        RuntimeEvent::ResourceBlocked { detail, pids } => {
            vec![InstallerEvent::blocked_with_processes(detail, pids.clone())]
        }
        RuntimeEvent::StagingStarted { id } => vec![InstallerEvent::Phase {
            state: format!("staging:{id}"),
        }],
        RuntimeEvent::StagingProgress { id, detail } => vec![InstallerEvent::Phase {
            state: format!("staging:{id}:{detail}"),
        }],
        RuntimeEvent::OperationStarted { id } => vec![InstallerEvent::Phase {
            state: format!("operation:{id}"),
        }],
        RuntimeEvent::Progress {
            completed,
            total,
            action,
        } => vec![InstallerEvent::progress(
            &zup_presentation::ProgressPresentation::new(*completed, *total, action),
        )],
        RuntimeEvent::PrerequisiteCheck {
            id,
            name,
            satisfied,
            version,
        } => vec![InstallerEvent::PrerequisiteCheck {
            id: id.clone(),
            name: name.clone(),
            satisfied: *satisfied,
            version: version.clone(),
        }],
        RuntimeEvent::PrerequisiteDownload {
            id,
            completed,
            total,
        } => vec![InstallerEvent::PrerequisiteDownload {
            id: id.clone(),
            completed: *completed,
            total: *total,
        }],
        RuntimeEvent::PrerequisiteInstall { id, name } => {
            vec![InstallerEvent::PrerequisiteInstall {
                id: id.clone(),
                name: name.clone(),
            }]
        }
        RuntimeEvent::RebootRequired { id, exit_code } => vec![InstallerEvent::RebootRequired {
            id: id.clone(),
            exit_code: *exit_code,
        }],
        RuntimeEvent::RollingBack => vec![InstallerEvent::Phase {
            state: "rolling_back".into(),
        }],
        RuntimeEvent::Completed { outcome } => {
            let outcome = if outcome == "committed" {
                zup_presentation::ProcessOutcome::Success
            } else {
                zup_presentation::ProcessOutcome::from_message(outcome)
            };
            vec![InstallerEvent::Completed { outcome }]
        }
        RuntimeEvent::Failed { kind, message } => {
            let outcome = match kind.as_str() {
                "installation_busy" => zup_presentation::ProcessOutcome::InstallationBusy,
                "recovery_required" | "committed_pending_reconciliation" => {
                    zup_presentation::ProcessOutcome::RecoveryRequired
                }
                "authorization_required" | "authorization_cancelled" => {
                    zup_presentation::ProcessOutcome::AuthorizationRequired
                }
                _ => zup_presentation::ProcessOutcome::from_message(message),
            };
            vec![InstallerEvent::Failed {
                outcome,
                code: outcome.code(),
                message: message.clone(),
                diagnostic: Some(zup_presentation::DiagnosticPresentation::from_message(
                    message,
                    *kind == "recovery_required",
                )),
            }]
        }
        RuntimeEvent::LogPath { .. } => Vec::new(),
    }
}
