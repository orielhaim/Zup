use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::{AppId, ComponentId, InstallScope, Installer, SelectedScope, TargetBuildPlan};
use zup_plan::InstallationRecord;
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
    pub ledger: Option<InstallationRecord>,
}

#[derive(Debug, Clone)]
pub struct OperationContext {
    pub scope: SelectedScope,
    pub state_root: PathBuf,
    pub ledger: Option<InstallationRecord>,
}

#[derive(Debug, Clone)]
pub struct SessionDiscovery {
    pub build: Arc<TargetBuildPlan>,
    pub installations: Vec<ScopedInstallation>,
    pub recoveries: std::collections::BTreeMap<
        SelectedScope,
        Result<Option<zup_core::RecoverySummary>, String>,
    >,
    pub observation_errors: std::collections::BTreeMap<SelectedScope, String>,
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
    recoveries: &std::collections::BTreeMap<
        SelectedScope,
        Result<Option<zup_core::RecoverySummary>, String>,
    >,
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
    if let Some(scope) = permitted.iter().find(|scope| {
        recoveries
            .get(scope)
            .is_some_and(|result| matches!(result, Ok(Some(_))))
    }) {
        return Ok(*scope);
    }
    let installed: Vec<SelectedScope> = permitted
        .iter()
        .copied()
        .filter(|scope| {
            installations.iter().any(|installation| {
                installation.scope == *scope
                    && installation
                        .ledger
                        .as_ref()
                        .is_some_and(InstallationRecord::is_installed)
            })
        })
        .collect();
    match installed.as_slice() {
        [scope, ..] => Ok(*scope),
        [] => installations
            .iter()
            .find(|installation| {
                installation.ledger.is_some() && permitted.contains(&installation.scope)
            })
            .map(|installation| installation.scope)
            .or_else(|| permitted.first().copied())
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

pub enum UpdatePreparation {
    Cancelled,
    UpToDate {
        version: String,
    },
    Available {
        candidate: zup_core::ReleaseCandidate,
    },
    Ready {
        build: Arc<TargetBuildPlan>,
    },
}

pub trait SessionHost {
    type Bound<'a>: OperationHost
    where
        Self: 'a;

    fn describe(&mut self, executable: &Path) -> Result<Arc<TargetBuildPlan>, String>;

    fn state_root(
        &self,
        installer: &Installer,
        scope: SelectedScope,
        hint: Option<&Path>,
    ) -> Result<PathBuf, String>;

    fn recovery_store(
        &self,
        context: &OperationContext,
        _app: &AppId,
    ) -> Result<zup_transaction::FilesystemTransactionStore, String> {
        Ok(zup_transaction::FilesystemTransactionStore::private_at(
            &context.state_root,
        ))
    }

    fn pending_recovery(
        &self,
        context: &OperationContext,
        app: &AppId,
    ) -> Result<Option<zup_core::RecoverySummary>, String> {
        self.recovery_store(context, app)?
            .pending_recovery(app, context.scope)
            .map(|target| target.map(|target| target.summary()))
            .map_err(|error| error.to_string())
    }

    fn observe_recovery(
        &self,
        discovery: &SessionDiscovery,
        context: &OperationContext,
        state: &mut HostState,
    ) {
        match self.pending_recovery(context, discovery.app_id())
        {
            Ok(Some(target)) => {
                state.set_recovery(target);
            }
            Ok(None) => {}
            Err(error) => state.refresh_required(format!("Protected deployment state could not be inspected: {error}. Close other maintenance processes and refresh. Use an authorized administrator session for machine state.")),
        }
    }

    fn active_launchers(
        &self,
        context: &OperationContext,
        app: &AppId,
    ) -> Result<Option<zup_core::ActiveLaunchers>, String> {
        let Some(installation) = context.ledger.as_ref() else {
            return Ok(None);
        };
        if installation.app_id != *app || installation.scope != context.scope {
            return Err("launcher installation identity mismatch".into());
        }
        self.recovery_store(context, app)?
            .active_launchers(installation)
    }

    fn open_state(
        &self,
        discovery: &SessionDiscovery,
        context: &OperationContext,
        preselected: Option<&[ComponentId]>,
    ) -> HostState {
        let mut state = initial_state(discovery, context, preselected);
        match discovery.recoveries.get(&context.scope) {
            Some(Ok(Some(target))) => state.set_recovery(target.clone()),
            Some(Err(error)) => state.refresh_required(format!("Protected recovery inspection failed: {error}. Refresh with the required authority or use the source-free recovery console.")),
            Some(Ok(None)) => {}
            None => self.observe_recovery(discovery, context, &mut state),
        }
        if state.snapshot().recovery.is_none()
            && let Some(error) = discovery.observation_errors.get(&context.scope)
        {
            state.refresh_required(format!("The installation ledger could not be observed: {error}. Protected journal recovery was inspected independently. Refresh or inspect the journal through the recovery console."));
        }
        state
    }

    fn content_executable(&self, executable: &Path) -> PathBuf {
        executable.to_path_buf()
    }

    fn observe(
        &self,
        installer: &Installer,
        scope: SelectedScope,
        state_root: Option<&Path>,
    ) -> Result<OperationContext, String>;

    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        let build = self.describe(executable)?;
        if launch.state_root.is_some()
            && launch.scope.is_none()
            && build.installer.install.scope.scopes().len() > 1
        {
            return Err("a custom state root requires an explicit user or machine scope".into());
        }
        let mut installations = Vec::new();
        let mut recoveries = std::collections::BTreeMap::new();
        let mut observation_errors = std::collections::BTreeMap::new();
        for scope in build.installer.install.scope.scopes() {
            let state_root = launch
                .state_root
                .as_deref()
                .filter(|_| launch.scope.is_none_or(|selected| selected == *scope));
            let context = match self.observe(&build.installer, *scope, state_root) {
                Ok(context) => context,
                Err(error) => {
                    observation_errors.insert(*scope, error);
                    OperationContext {
                        scope: *scope,
                        state_root: self.state_root(&build.installer, *scope, state_root)?,
                        ledger: None,
                    }
                }
            };
            if launch.scope.is_none_or(|selected| selected == *scope) {
                recoveries.insert(
                    *scope,
                    self.pending_recovery(&context, &build.installer.app.id),
                );
            }
            installations.push(ScopedInstallation {
                scope: context.scope,
                state_root: context.state_root,
                ledger: context.ledger,
            });
        }
        Ok(SessionDiscovery {
            build,
            installations,
            recoveries,
            observation_errors,
        })
    }

    fn acquire_content(
        &mut self,
        _build: &mut Arc<TargetBuildPlan>,
        _context: &OperationContext,
        _operation: zup_runtime::Operation,
        _selection: &zup_runtime::OperationSelection,
        _events: &mut (dyn FnMut(zup_presentation::InstallerEvent) + Send),
    ) -> Result<(), String> {
        Ok(())
    }

    fn supports_updates(&self) -> bool {
        false
    }

    fn prepare_update(
        &mut self,
        _build: &Arc<TargetBuildPlan>,
        _context: &OperationContext,
        _source: Option<&Path>,
        _request: &zup_runtime::UpdateRequest,
        _cancellation: zup_runtime::CancellationHandle,
        _events: &mut (dyn FnMut(zup_presentation::InstallerEvent) + Send),
    ) -> Result<UpdatePreparation, String> {
        Err("this runtime does not support authenticated updates".into())
    }

    fn release_identity(&self) -> Option<zup_core::ReleaseIdentity> {
        None
    }

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

    fn recover<'a>(
        &'a self,
        _context: &'a OperationContext,
        _target: zup_core::RecoverySummary,
        _request: zup_transaction::RecoveryRequest,
        _control: &'a zup_runtime::OperationControl,
    ) -> zup_runtime::RuntimeFuture<'a, Result<InstallOutcome, zup_runtime::SessionError>> {
        Box::pin(async {
            Err(zup_runtime::SessionError::Transaction(
                "explicit recovery is not supported here".into(),
            ))
        })
    }

    fn refresh_installation(
        &mut self,
        _context: &OperationContext,
    ) -> Result<Option<Arc<TargetBuildPlan>>, String> {
        Ok(None)
    }

    fn after_commit(&self, _context: &OperationContext) -> Result<(), String> {
        Ok(())
    }

    fn update(
        &mut self,
        _context: &OperationContext,
        _request: &zup_runtime::UpdateRequest,
        _control: &zup_runtime::OperationControl,
        _status: &mut (dyn FnMut(UpdateState) + Send),
    ) -> Result<crate::SessionOutcome, String> {
        Err("updates are not supported here".into())
    }

    fn open_log(&self, path: &str) -> Result<(), String> {
        opener::open(path).map_err(|error| error.to_string())
    }

    fn copy_diagnostics(&self, _text: &str) {}
}

pub fn session_capabilities(
    installer: &Installer,
    context: &OperationContext,
) -> zup_preset_protocol::Capabilities {
    let installed = context
        .ledger
        .as_ref()
        .is_some_and(InstallationRecord::is_installed);
    let capabilities = zup_preset_host::surface::capabilities(installer, installed);
    if active_launchers(context).is_empty() {
        capabilities
    } else {
        capabilities.with(zup_preset_protocol::Capability::Launch)
    }
}

fn active_launchers(context: &OperationContext) -> Vec<zup_preset_host::Launchable> {
    context
        .ledger
        .as_ref()
        .filter(|record| record.is_installed())
        .into_iter()
        .flat_map(|record| {
            record
                .current
                .requirements
                .launchers
                .iter()
                .filter(|launcher| {
                    matches!(
                        record.current.resources.get(&launcher.key),
                        Some(zup_plan::OwnedResource::Launcher {
                            installed: zup_plan::LauncherState::Launcher { .. },
                            ..
                        })
                    )
                })
                .map(|launcher| zup_preset_host::Launchable {
                    target: zup_preset_protocol::LaunchTarget {
                        name: launcher.name.to_string(),
                    },
                    component: None,
                })
        })
        .collect()
}

pub fn initial_state(
    discovery: &SessionDiscovery,
    context: &OperationContext,
    preselected: Option<&[ComponentId]>,
) -> HostState {
    let installer = discovery.installer();
    let launchers = active_launchers(context);
    let capabilities = session_capabilities(installer, context);
    let state = match context
        .ledger
        .as_ref()
        .filter(|record| record.is_installed())
    {
        Some(ledger) => {
            let maintenance =
                zup_preset_host::surface::maintenance_state(installer, ledger, context.scope);
            HostState::maintenance(
                zup_preset_host::surface::product(installer),
                maintenance,
                capabilities,
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
                capabilities,
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
        RuntimeEvent::PlanPrepared { plan } => {
            match zup_presentation::PlanPreview::from_transaction_plan(
                plan,
                plan.requirements.scope,
                None,
            ) {
                Ok(preview) => vec![InstallerEvent::PlanPrepared {
                    preview: Box::new(preview),
                }],
                Err(error) => vec![InstallerEvent::Failed {
                    outcome: zup_presentation::ProcessOutcome::VerificationFailure,
                    code: 1,
                    message: error.to_string(),
                    diagnostic: None,
                }],
            }
        }
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
            phase,
            ..
        } => vec![InstallerEvent::progress(
            &zup_presentation::ProgressPresentation::new(*phase, *completed, *total, action),
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
            let outcome = zup_presentation::ProcessOutcome::from(*outcome);
            vec![InstallerEvent::Completed { outcome }]
        }
        RuntimeEvent::Failed { kind, message } => {
            let outcome = zup_presentation::ProcessOutcome::from(*kind);
            vec![InstallerEvent::Failed {
                outcome,
                code: outcome.code(),
                message: message.clone(),
                diagnostic: Some(zup_presentation::DiagnosticPresentation::for_outcome(
                    outcome, message,
                )),
            }]
        }
        RuntimeEvent::LogPath { .. } => Vec::new(),
    }
}
