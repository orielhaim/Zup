use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zup_plan::{DeploymentEffects, DeploymentPlan, compile_deployment};

use zup_core::{ComponentId, ResourceKey, SelectedScope, TargetBuildPlan, Template};
use zup_plan::{
    CancellationQuery, DesiredState, ExecutionPlan, HostSnapshot, InstallationRecord,
    LifecycleAction, PlanRequest, PlannedInstallation, plan_lifecycle,
};

use crate::BootstrapRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRequest {
    Check,
    Install {
        expected: Option<zup_core::ReleaseCandidate>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Cleanup {
        keep: u16,
    },
    Install,
    Upgrade,
    Downgrade,
    Rollback {
        revision: zup_core::ActivationRevision,
    },
    Modify,
    Repair {
        force: bool,
    },
    Uninstall,
    Apply,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cleanup { .. } => "cleanup",
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::Rollback { .. } => "rollback",
            Self::Modify => "modify",
            Self::Repair { .. } => "repair",
            Self::Uninstall => "uninstall",
            Self::Apply => "apply",
        }
    }
}

impl From<LifecycleAction> for Operation {
    fn from(action: LifecycleAction) -> Self {
        match action {
            LifecycleAction::Cleanup { keep } => Self::Cleanup { keep },
            LifecycleAction::Install => Self::Install,
            LifecycleAction::Upgrade => Self::Upgrade,
            LifecycleAction::Downgrade => Self::Downgrade,
            LifecycleAction::Rollback { revision } => Self::Rollback { revision },
            LifecycleAction::Modify => Self::Modify,
            LifecycleAction::Repair { force } => Self::Repair { force },
            LifecycleAction::Uninstall => Self::Uninstall,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct OperationSelection {
    pub deployment: Option<zup_plan::DeploymentSelection>,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationError {
    Cancelled,
    RecoveryRequired(zup_core::TransactionId),
    Downgrade {
        installed: semver::Version,
        requested: semver::Version,
    },
    Refused {
        stage: &'static str,
        message: String,
    },
}

impl OperationError {
    pub fn refused(stage: &'static str, message: impl std::fmt::Display) -> Self {
        Self::Refused {
            stage,
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => out.write_str("planning cancelled"),
            Self::RecoveryRequired(transaction) => {
                write!(out, "transaction {transaction} requires explicit recovery")
            }
            Self::Downgrade {
                installed,
                requested,
            } => write!(out, "downgrade from {installed} to {requested} is refused"),
            Self::Refused { stage, message } => write!(out, "{stage}: {message}"),
        }
    }
}

impl std::error::Error for OperationError {}
impl From<zup_plan::PlanError> for OperationError {
    fn from(error: zup_plan::PlanError) -> Self {
        match error {
            zup_plan::PlanError::Cancelled => Self::Cancelled,
            error => Self::refused("plan", error),
        }
    }
}

pub fn resolve_operation(
    operation: Operation,
    installed: Option<&semver::Version>,
    package: &semver::Version,
) -> Result<LifecycleAction, OperationError> {
    match operation {
        Operation::Cleanup { keep } => Ok(LifecycleAction::Cleanup { keep }),
        Operation::Install => Ok(LifecycleAction::Install),
        Operation::Upgrade => Ok(LifecycleAction::Upgrade),
        Operation::Downgrade => Ok(LifecycleAction::Downgrade),
        Operation::Rollback { revision } => Ok(LifecycleAction::Rollback { revision }),
        Operation::Modify => Ok(LifecycleAction::Modify),
        Operation::Repair { force } => Ok(LifecycleAction::Repair { force }),
        Operation::Uninstall => Ok(LifecycleAction::Uninstall),
        Operation::Apply => {
            let Some(installed) = installed else {
                return Ok(LifecycleAction::Install);
            };
            match package.cmp(installed) {
                std::cmp::Ordering::Greater => Ok(LifecycleAction::Upgrade),
                std::cmp::Ordering::Equal => Ok(LifecycleAction::Modify),
                std::cmp::Ordering::Less => Err(OperationError::Downgrade {
                    installed: installed.clone(),
                    requested: package.clone(),
                }),
            }
        }
    }
}

pub fn maintenance_operations(
    installed: &semver::Version,
    package: &semver::Version,
) -> Vec<Operation> {
    let mut operations = match package.cmp(installed) {
        std::cmp::Ordering::Greater => vec![Operation::Upgrade],
        std::cmp::Ordering::Equal => {
            vec![Operation::Modify, Operation::Repair { force: false }]
        }
        std::cmp::Ordering::Less => vec![Operation::Downgrade],
    };
    operations.push(Operation::Cleanup { keep: 1 });
    operations.push(Operation::Uninstall);
    operations
}

pub struct OperationInput<'a> {
    pub build: &'a TargetBuildPlan,
    pub installer: &'a Path,
    pub scope: SelectedScope,
    pub operation: Operation,
    pub selection: OperationSelection,
    pub prior: Option<&'a InstallationRecord>,
    pub state_root: &'a Path,
    pub cancellation: &'a dyn CancellationQuery,
}

pub struct ContentAccess<'a> {
    pub installer: &'a Path,
    pub state_root: &'a Path,
    pub scope: SelectedScope,
    pub action: LifecycleAction,
    pub prior: Option<&'a InstallationRecord>,
    pub maintenance: Option<&'a zup_plan::GenerationSelection>,
}

pub trait OperationHost {
    fn collected_deployments(
        &self,
        _input: &OperationInput,
        _record: &InstallationRecord,
    ) -> Result<Vec<zup_plan::RetiredDeploymentSelection>, String> {
        Ok(Vec::new())
    }

    fn cleanup_candidates(
        &self,
        _input: &OperationInput,
        _record: &InstallationRecord,
        _retiring: Option<&zup_plan::RetentionChange>,
    ) -> Result<Vec<zup_plan::CleanupOperation>, String> {
        Err("this host has no protected cleanup authority".into())
    }

    fn persistent_claims(
        &self,
        _input: &OperationInput,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, OperationError> {
        Ok(None)
    }
    fn validate_context(&self, _input: &OperationInput) -> Result<(), OperationError> {
        Ok(())
    }

    fn plan_build(
        &self,
        build: &TargetBuildPlan,
        request: &PlanRequest,
        cancellation: &dyn CancellationQuery,
    ) -> Result<PlannedInstallation, OperationError> {
        let planning = zup_core::BuildPlan {
            targets: vec![build.clone()],
        };
        zup_plan::plan(&planning, request, None, cancellation).map_err(OperationError::from)
    }

    fn resolve_target(&self, install: &DesiredState) -> Result<zup_plan::BoundDeployment, String>;

    fn observe(
        &self,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&InstallationRecord>,
    ) -> Result<(Option<HostSnapshot>, BTreeMap<ResourceKey, bool>), String>;

    fn compile(
        &self,
        execution: &ExecutionPlan,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&InstallationRecord>,
    ) -> Result<DeploymentEffects, String>;

    fn data_bindings(
        &self,
        target: &zup_plan::BoundDeployment,
    ) -> Result<BTreeMap<zup_core::ResourceId, zup_plan::PersistentBinding>, String> {
        target
            .data
            .iter()
            .map(|directory| {
                let zup_core::PersistentLocation::Path {
                    path,
                    access: zup_core::PersistentAccess::Owner,
                } = &directory.location
                else {
                    return Err(format!(
                        "data {} needs a native service data provider",
                        directory.id
                    ));
                };
                Ok((
                    directory.id.clone(),
                    zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding {
                        path: path.clone(),
                        writers: Vec::new(),
                    }),
                ))
            })
            .collect()
    }

    fn attach_content(
        &self,
        target: &mut zup_plan::BoundDeployment,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String>;

    fn install_directory(
        &self,
        explicit: Option<&Path>,
        prior: Option<&InstallationRecord>,
        allowed: bool,
        repair: bool,
    ) -> Result<Option<Template>, String> {
        choose_install_directory(explicit, prior, allowed, repair)
            .map_err(|error| error.to_string())
    }

    fn prepare_bootstrap(
        &self,
        _install: &DesiredState,
    ) -> Result<Option<BootstrapRequest>, String> {
        Ok(None)
    }
}

#[derive(Debug)]
pub struct PlannedOperation {
    pub install_plan: DesiredState,
    pub target_plan: zup_plan::BoundDeployment,
    pub generated_files: Vec<zup_plan::GeneratedFile>,
    pub bootstrap: Option<BootstrapRequest>,
}

#[derive(Debug)]
pub enum OperationBody {
    Cleanup {
        release: Option<zup_core::ReleaseIdentity>,
    },
    Planned(Box<PlannedOperation>),
    Retained {
        planned: Box<PlannedOperation>,
        release: Option<zup_core::ReleaseIdentity>,
    },
    Uninstall,
}

#[derive(Debug)]
pub struct PreparedOperation {
    pub operation: Operation,
    pub action: LifecycleAction,
    pub transaction_plan: DeploymentPlan,
    pub selection: OperationSelection,
    pub body: OperationBody,
}

impl PreparedOperation {
    pub fn target_plan(&self) -> Option<&zup_plan::BoundDeployment> {
        match &self.body {
            OperationBody::Planned(planned) | OperationBody::Retained { planned, .. } => {
                Some(&planned.target_plan)
            }
            OperationBody::Uninstall | OperationBody::Cleanup { .. } => None,
        }
    }

    pub fn install_plan(&self) -> Option<&DesiredState> {
        match &self.body {
            OperationBody::Planned(planned) | OperationBody::Retained { planned, .. } => {
                Some(&planned.install_plan)
            }
            OperationBody::Uninstall | OperationBody::Cleanup { .. } => None,
        }
    }

    pub fn generated_files(&self) -> &[zup_plan::GeneratedFile] {
        match &self.body {
            OperationBody::Planned(planned) | OperationBody::Retained { planned, .. } => {
                &planned.generated_files
            }
            OperationBody::Uninstall | OperationBody::Cleanup { .. } => &[],
        }
    }

    pub fn bootstrap(&self) -> Option<&BootstrapRequest> {
        match &self.body {
            OperationBody::Planned(planned) => planned.bootstrap.as_ref(),
            OperationBody::Retained { .. }
            | OperationBody::Uninstall
            | OperationBody::Cleanup { .. } => None,
        }
    }
}

pub fn prepare_operation<H: OperationHost>(
    input: &OperationInput,
    host: &H,
) -> Result<PreparedOperation, OperationError> {
    if let Some(deployment) = &input.selection.deployment {
        deployment
            .validate(input.prior)
            .map_err(|error| OperationError::refused("deployment selection", error))?;
    }
    if input.cancellation.is_cancelled() {
        return Err(OperationError::Cancelled);
    }
    input
        .build
        .installer
        .install
        .scope
        .select(Some(input.scope))
        .map_err(|error| OperationError::refused("scope", error))?;
    let capabilities = zup_plan::capabilities(&input.build.installer.target).ok_or_else(|| {
        OperationError::refused(
            "capabilities",
            format!("unsupported target `{}`", input.build.installer.target),
        )
    })?;
    if !capabilities.scopes.supports(input.scope)
        || !capabilities
            .frontends
            .contains(&input.build.installer.frontend)
    {
        return Err(OperationError::refused(
            "capabilities",
            "unsupported target scope or frontend",
        ));
    }
    host.validate_context(input)?;
    if let Operation::Cleanup { keep } = input.operation {
        if !input.selection.enable.is_empty()
            || !input.selection.disable.is_empty()
            || input.selection.install_directory.is_some()
        {
            return Err(OperationError::refused(
                "cleanup",
                "cleanup preserves the installed selection and location",
            ));
        }
        let record = input
            .prior
            .ok_or_else(|| OperationError::refused("cleanup", "installation not found"))?;
        if record.app_id != input.build.installer.app.id
            || record.target != input.build.installer.target
            || record.scope != input.scope
        {
            return Err(OperationError::refused(
                "cleanup",
                "installation belongs to a different application, target or scope",
            ));
        }
        let action = LifecycleAction::Cleanup { keep };
        let mut effects = DeploymentEffects::new(record.id, record.current.requirements.clone());
        effects.transition = zup_plan::DeploymentTransition::Maintain;
        effects.observed = Some(record.current.selection());
        effects.observed_transaction = Some(record.commit.transaction);
        effects.install_directory = Some(record.directory.clone());
        effects.selected_components = record.current.selected_components.clone();
        effects.preset = record.current.preset.clone();
        effects.retention = zup_plan::RetentionChange::keeping_previous(record, keep);
        let forget = host
            .collected_deployments(input, record)
            .map_err(|error| OperationError::refused("cleanup", error))?;
        if !forget.is_empty() {
            effects
                .retention
                .get_or_insert_with(|| zup_plan::RetentionChange {
                    retire: Vec::new(),
                    forget: Vec::new(),
                })
                .forget = forget;
        }

        effects.cleanup = host
            .cleanup_candidates(input, record, effects.retention.as_ref())
            .map_err(|error| OperationError::refused("cleanup", error))?;
        let transaction_plan = compile_deployment(&effects)
            .map_err(|error| OperationError::refused("cleanup", error))?;
        return Ok(PreparedOperation {
            operation: input.operation,
            action,
            transaction_plan,
            selection: input.selection.clone(),
            body: OperationBody::Cleanup {
                release: record.current.release.clone(),
            },
        });
    }
    if let Operation::Rollback { revision } = input.operation {
        return prepare_retained_operation(input, host, revision);
    }
    let claims = host.persistent_claims(input)?;
    let installer = &input.build.installer;
    if matches!(input.operation, Operation::Repair { .. })
        && (!input.selection.enable.is_empty()
            || !input.selection.disable.is_empty()
            || input.selection.install_directory.is_some())
    {
        return Err(OperationError::refused(
            "selection",
            "repair uses the committed component selection and install location",
        ));
    }
    let action = resolve_operation(
        input.operation,
        input
            .prior
            .filter(|record| record.is_installed())
            .map(|record| &record.current.version),
        &installer.app.version,
    )?;
    if action == LifecycleAction::Uninstall {
        let ledger = input
            .prior
            .ok_or_else(|| OperationError::refused("resolve", "installation not found"))?;
        if ledger.target != installer.target {
            return Err(OperationError::refused(
                "resolve",
                format!(
                    "ledger target `{}` does not match this package's target `{}`",
                    ledger.target, installer.target
                ),
            ));
        }
        let target = None;
        let (snapshot, owned_matches) = host
            .observe(target.as_ref(), Some(ledger))
            .map_err(|message| OperationError::refused("observe", message))?;
        let execution = plan_lifecycle(
            action,
            None,
            snapshot.as_ref(),
            Some(ledger),
            &owned_matches,
        )
        .map_err(|error| OperationError::refused("lifecycle", error))?;
        let mut transaction_input = host
            .compile(&execution, target.as_ref(), Some(ledger))
            .map_err(|message| OperationError::refused("transaction", message))?;
        transaction_input
            .bind_persistent(None, Some(ledger), claims.as_ref(), &BTreeMap::new())
            .map_err(|error| OperationError::refused("persistent data", error))?;
        let transaction_plan = compile_deployment(&transaction_input)
            .map_err(|error| OperationError::refused("transaction", error))?;
        return Ok(PreparedOperation {
            operation: input.operation,
            action,
            transaction_plan,
            selection: input.selection.clone(),
            body: OperationBody::Uninstall,
        });
    }
    let install_directory = host
        .install_directory(
            input.selection.install_directory.as_deref(),
            input.prior,
            installer.install.allow_directory_override,
            matches!(action, LifecycleAction::Repair { .. }),
        )
        .map_err(|message| OperationError::refused("selection", message))?;
    let mut plan_request = plan_request(
        installer,
        input.scope,
        input.prior,
        &input.selection,
        action,
        install_directory,
    )?;
    if let Some(deployment) = &input.selection.deployment {
        if matches!(action, LifecycleAction::Repair { .. })
            && deployment.generation.as_ref().is_some_and(|generation| {
                generation.policy != zup_plan::GenerationPolicy::Reconstruct
            })
        {
            return Err(OperationError::refused(
                "deployment selection",
                "repair requires a new physical materialization",
            ));
        }
        deployment.apply_to(&mut plan_request);
    }
    if let Some(claims) = &claims {
        if input
            .prior
            .is_some_and(|prior| prior.id != claims.installation)
            || input
                .selection
                .install_directory
                .as_ref()
                .is_some_and(|directory| {
                    !zup_core::TargetPath::new(installer.target, directory.to_string_lossy())
                        .is_ok_and(|directory| directory.equivalent(&claims.directory))
                })
        {
            return Err(OperationError::refused(
                "persistent data",
                "retained data belongs to a different installation directory",
            ));
        }
        plan_request.installation = claims.installation;
        plan_request.install_directory = Some(zup_plan::InstallDirectory::Installed(
            Template::parse(claims.directory.as_str())
                .map_err(|error| OperationError::refused("persistent data", error))?,
        ));
    }
    let mut planned = host.plan_build(input.build, &plan_request, input.cancellation)?;
    let explicit_reuse = input
        .selection
        .deployment
        .as_ref()
        .and_then(|selection| selection.generation.as_ref())
        .filter(|selection| selection.policy == zup_plan::GenerationPolicy::ReuseVerified);
    let reused = explicit_reuse.and_then(|selected| {
        input
            .prior?
            .current
            .materialization
            .iter()
            .chain(
                input
                    .prior?
                    .retained
                    .iter()
                    .filter_map(|snapshot| snapshot.materialization.as_ref()),
            )
            .find(|generation| generation.id() == selected.materialization)
    });
    if explicit_reuse.is_some() && reused.is_none() {
        return Err(OperationError::refused(
            "generation",
            "selected materialization is not retained by this installation",
        ));
    }
    if let Some(reused) = reused {
        let generation = planned.plan.generation.as_mut().ok_or_else(|| {
            OperationError::refused(
                "generation",
                "retained application content differs from requested content",
            )
        })?;
        if reused.installation() != plan_request.installation
            || reused.descriptor() != &generation.descriptor
        {
            return Err(OperationError::refused(
                "generation",
                "retained application content differs from requested content",
            ));
        }
        generation.id = reused.id();
        generation.directory = Template::parse(reused.directory().as_str())
            .map_err(|error| OperationError::refused("generation", error))?;
        generation.preparation = zup_plan::GenerationPreparation::ReuseVerified;
    }
    let mut target = host
        .resolve_target(&planned.plan)
        .map_err(|message| OperationError::refused("target", message))?;
    host.attach_content(
        &mut target,
        &ContentAccess {
            installer: input.installer,
            state_root: input.state_root,
            scope: input.scope,
            action,
            prior: input.prior,
            maintenance: plan_request.maintenance.as_ref(),
        },
        input.build.installer.preset.as_ref(),
    )
    .map_err(|message| OperationError::refused("content", message))?;
    if action == LifecycleAction::Modify && plan_request.maintenance.is_none() {
        reuse_unchanged_maintenance(&mut target, input.prior);
    }
    let (snapshot, owned_matches) = host
        .observe(Some(&target), input.prior)
        .map_err(|message| OperationError::refused("observe", message))?;
    let snapshot = snapshot
        .ok_or_else(|| OperationError::refused("observe", "a transaction observes its target"))?;
    let execution = plan_lifecycle(
        action,
        Some(&target),
        Some(&snapshot),
        input.prior,
        &owned_matches,
    )
    .map_err(|error| OperationError::refused("lifecycle", error))?;
    let mut transaction_input = host
        .compile(&execution, Some(&target), input.prior)
        .map_err(|message| OperationError::refused("transaction", message))?;
    transaction_input
        .bind_persistent(
            Some(&target),
            input.prior,
            claims.as_ref(),
            &host
                .data_bindings(&target)
                .map_err(|error| OperationError::refused("persistent data", error))?,
        )
        .map_err(|error| OperationError::refused("persistent data", error))?;
    let transaction_plan = compile_deployment(&transaction_input)
        .map_err(|error| OperationError::refused("transaction", error))?;
    let bootstrap = host
        .prepare_bootstrap(&planned.plan)
        .map_err(|message| OperationError::refused("prerequisites", message))?;
    Ok(PreparedOperation {
        operation: input.operation,
        action,
        transaction_plan,
        selection: input.selection.clone(),
        body: OperationBody::Planned(Box::new(PlannedOperation {
            install_plan: planned.plan,
            target_plan: target,
            generated_files: planned.generated_files,
            bootstrap,
        })),
    })
}

fn prepare_retained_operation<H: OperationHost>(
    input: &OperationInput,
    host: &H,
    revision: zup_core::ActivationRevision,
) -> Result<PreparedOperation, OperationError> {
    if !input.selection.enable.is_empty()
        || !input.selection.disable.is_empty()
        || input.selection.install_directory.is_some()
    {
        return Err(OperationError::refused(
            "rollback",
            "rollback preserves the retained component selection and install directory",
        ));
    }
    let record = input
        .prior
        .ok_or_else(|| OperationError::refused("rollback", "installation not found"))?;
    if record.app_id != input.build.installer.app.id
        || record.target != input.build.installer.target
        || record.scope != input.scope
    {
        return Err(OperationError::refused(
            "rollback",
            "retained deployment belongs to a different application, scope or target",
        ));
    }
    let desired = record
        .retained_deployment(revision)
        .map_err(|error| OperationError::refused("rollback", error))?;
    let target = host
        .resolve_target(&desired)
        .map_err(|error| OperationError::refused("resolve", error))?;
    let (snapshot, owned) = host
        .observe(Some(&target), Some(record))
        .map_err(|error| OperationError::refused("observe", error))?;
    let action = LifecycleAction::Rollback { revision };
    let execution = plan_lifecycle(
        action,
        Some(&target),
        snapshot.as_ref(),
        Some(record),
        &owned,
    )
    .map_err(|error| OperationError::refused("rollback", error))?;
    let mut effects = host
        .compile(&execution, Some(&target), Some(record))
        .map_err(|error| OperationError::refused("transaction", error))?;
    effects
        .bind_persistent(
            Some(&target),
            Some(record),
            None,
            &host
                .data_bindings(&target)
                .map_err(|error| OperationError::refused("persistent data", error))?,
        )
        .map_err(|error| OperationError::refused("persistent data", error))?;
    let transaction_plan = compile_deployment(&effects)
        .map_err(|error| OperationError::refused("transaction", error))?;
    Ok(PreparedOperation {
        operation: input.operation,
        action,
        transaction_plan,
        selection: input.selection.clone(),
        body: OperationBody::Retained {
            planned: Box::new(PlannedOperation {
                install_plan: desired,
                target_plan: target,
                generated_files: Vec::new(),
                bootstrap: None,
            }),
            release: record
                .retained
                .iter()
                .find(|snapshot| snapshot.revision == revision)
                .and_then(|snapshot| snapshot.release.clone()),
        },
    })
}

pub fn choose_install_directory(
    explicit: Option<&Path>,
    prior: Option<&InstallationRecord>,
    allowed: bool,
    repair: bool,
) -> Result<Option<Template>, OperationError> {
    if repair {
        return Ok(persisted_install_directory(prior));
    }
    choose_explicit_directory(explicit, prior, allowed)
}

fn choose_explicit_directory(
    explicit: Option<&Path>,
    prior: Option<&InstallationRecord>,
    allowed: bool,
) -> Result<Option<Template>, OperationError> {
    if let Some(path) = explicit {
        if !allowed {
            return Err(OperationError::refused(
                "selection",
                "this application does not allow choosing an install directory",
            ));
        }
        return install_directory_template(path).map(Some);
    }
    Ok(persisted_install_directory(prior))
}

pub fn persisted_install_directory(prior: Option<&InstallationRecord>) -> Option<Template> {
    prior
        .map(|ledger| &ledger.directory)
        .and_then(|path| Template::parse(&path.to_string()).ok())
}

pub fn install_directory_template(path: &Path) -> Result<Template, OperationError> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(OperationError::refused(
            "selection",
            "install directory must not contain template variables",
        ));
    }
    Template::parse(&value).map_err(|error| OperationError::refused("selection", error))
}

pub fn plan_request(
    installer: &zup_core::Installer,
    scope: SelectedScope,
    prior: Option<&InstallationRecord>,
    selection: &OperationSelection,
    action: LifecycleAction,
    install_directory: Option<Template>,
) -> Result<PlanRequest, OperationError> {
    let mut request = PlanRequest::new(installer.target, scope);
    if matches!(action, LifecycleAction::Repair { .. }) {
        request.generation_policy = zup_plan::GenerationPolicy::Reconstruct;
    }
    if let Some(previous) = prior {
        previous
            .validate()
            .map_err(|error| OperationError::refused("installation", error))?;
        request.installation = previous.id;
        request.previous = Some(previous.current.selection());
    }
    request.install_directory = install_directory.map(|directory| {
        if selection.install_directory.is_none() && prior.is_some() {
            zup_plan::InstallDirectory::Installed(directory)
        } else {
            zup_plan::InstallDirectory::Override(directory)
        }
    });
    if matches!(
        action,
        LifecycleAction::Upgrade
            | LifecycleAction::Downgrade
            | LifecycleAction::Rollback { .. }
            | LifecycleAction::Modify
            | LifecycleAction::Repair { .. }
    ) {
        let previous =
            prior.ok_or_else(|| OperationError::refused("resolve", "installation not found"))?;
        for component in &installer.components {
            if previous.current.selected_components.contains(&component.id) {
                request.components.enable.insert(component.id.clone());
            } else if !component.required {
                request.components.disable.insert(component.id.clone());
            }
        }
    }
    apply_selection(&mut request, selection)?;
    Ok(request)
}

fn apply_selection(
    request: &mut PlanRequest,
    selection: &OperationSelection,
) -> Result<(), OperationError> {
    if let Some(id) = selection
        .enable
        .iter()
        .find(|id| selection.disable.contains(id))
    {
        return Err(OperationError::refused(
            "selection",
            format!("component `{id}` is both enabled and disabled"),
        ));
    }
    for raw in &selection.enable {
        let id = ComponentId::new(raw).map_err(|error| {
            OperationError::refused("selection", format!("component {raw}: {error}"))
        })?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in &selection.disable {
        let id = ComponentId::new(raw).map_err(|error| {
            OperationError::refused("selection", format!("component {raw}: {error}"))
        })?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }
    Ok(())
}

fn reuse_unchanged_maintenance(
    target: &mut zup_plan::BoundDeployment,
    record: Option<&InstallationRecord>,
) {
    let Some(record) = record.filter(|record| {
        record.is_installed()
            && record.id == target.installation
            && record.app_id == target.app.id
            && record.scope == target.scope
            && record.target == target.target
            && record.directory == target.install_directory
    }) else {
        return;
    };
    let (Some(previous), Some(generation)) = (
        record.current.maintenance.as_ref(),
        target.maintenance.as_mut(),
    ) else {
        return;
    };
    if generation.preparation == zup_plan::GenerationPreparation::Materialize
        && generation.descriptor == *previous.descriptor()
        && target.preset == record.current.preset
    {
        generation.id = previous.id();
        generation.directory = previous.directory().clone();
        generation.preparation = zup_plan::GenerationPreparation::ReuseVerified;
        generation.sources.clear();
    }
}

pub fn attach_maintenance_copy(
    target: &mut zup_plan::BoundDeployment,
    content: &ContentAccess,
    source_name: &str,
) -> Result<zup_plan::GenerationPreparation, OperationError> {
    let selected_snapshot = content.prior.and_then(|record| {
        content
            .maintenance
            .and_then(|selection| {
                std::iter::once(&record.current)
                    .chain(record.retained.iter())
                    .find(|snapshot| {
                        snapshot
                            .maintenance
                            .as_ref()
                            .is_some_and(|generation| generation.id() == selection.materialization)
                    })
            })
            .or_else(|| content.maintenance.is_none().then_some(&record.current))
    });
    let previous = selected_snapshot.and_then(|snapshot| snapshot.maintenance.as_ref());
    let reuse = content
        .maintenance
        .is_some_and(|selection| selection.policy == zup_plan::GenerationPolicy::ReuseVerified)
        || content.maintenance.is_none()
            && matches!(content.action, LifecycleAction::Repair { .. });
    if reuse {
        let previous = previous.ok_or_else(|| {
            OperationError::refused(
                "content",
                "no installed maintenance generation is available to verify",
            )
        })?;
        if content
            .maintenance
            .is_some_and(|selection| selection.materialization != previous.id())
        {
            return Err(OperationError::refused(
                "content",
                "maintenance selection does not identify the installed generation",
            ));
        }
        target.maintenance = Some(zup_plan::PlannedGeneration {
            installation: target.installation,
            id: previous.id(),
            descriptor: previous.descriptor().clone(),
            directory: previous.directory().clone(),
            publication: zup_plan::GenerationPublication::Maintenance,
            preparation: zup_plan::GenerationPreparation::ReuseVerified,
            sources: BTreeMap::new(),
        });
        target.preset = selected_snapshot.and_then(|snapshot| snapshot.preset.clone());
        target.requirements.preset = target.preset.clone();
        return Ok(zup_plan::GenerationPreparation::ReuseVerified);
    }
    if matches!(content.action, LifecycleAction::Repair { .. }) {
        return Err(OperationError::refused(
            "content",
            "repair must verify the installed maintenance generation",
        ));
    }
    let id = content
        .maintenance
        .map_or_else(zup_core::MaterializationId::new_v7, |selection| {
            selection.materialization
        });
    if previous.is_some_and(|previous| previous.id() == id) {
        return Err(OperationError::refused(
            "content",
            "new maintenance content cannot overwrite a committed generation",
        ));
    }
    let file = std::fs::File::open(content.installer).map_err(|error| {
        OperationError::refused("content", format!("installer executable: {error}"))
    })?;
    let (size, sha256) = zup_core::hash_reader(file).map_err(|error| {
        OperationError::refused("content", format!("installer executable: {error}"))
    })?;
    let directory = target
        .install_directory
        .join(format!(
            "{}/{id}",
            zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY
        ))
        .map_err(|error| OperationError::refused("content", error))?;
    target.maintenance = Some(zup_plan::PlannedGeneration {
        installation: target.installation,
        id,
        descriptor: zup_core::GenerationDescriptor::new(target.target, vec![])
            .map_err(|error| OperationError::refused("content", error))?,
        directory,
        publication: zup_plan::GenerationPublication::Maintenance,
        preparation: zup_plan::GenerationPreparation::Materialize,
        sources: BTreeMap::new(),
    });
    attach_maintenance_member(
        target,
        zup_core::MaintenanceArtifact::Runtime,
        zup_core::GenerationFile {
            path: zup_core::RelativePath::new(format!(
                "maintenance{}",
                target.target.executable_suffix()
            ))
            .map_err(|error| OperationError::refused("content", error))?,
            size,
            sha256,
            executable: true,
        },
        zup_core::RelativePath::new(source_name)
            .map_err(|error| OperationError::refused("content", error))?,
    )?;
    Ok(zup_plan::GenerationPreparation::Materialize)
}

fn attach_maintenance_member(
    target: &mut zup_plan::BoundDeployment,
    artifact: zup_core::MaintenanceArtifact,
    member: zup_core::GenerationFile,
    source_relative: zup_core::RelativePath,
) -> Result<(), OperationError> {
    let generation = target.maintenance.as_mut().ok_or_else(|| {
        OperationError::refused("content", "maintenance generation was not prepared")
    })?;
    if generation.preparation != zup_plan::GenerationPreparation::Materialize {
        return Err(OperationError::refused(
            "content",
            "a committed maintenance generation cannot be modified",
        ));
    }
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Maintenance { artifact },
    };
    if generation.sources.values().any(|source| source.key == key) {
        return Err(OperationError::refused(
            "content",
            "duplicate maintenance artifact identity",
        ));
    }
    let mut files = generation.descriptor.files().to_vec();
    let path = member.path.clone();
    files.push(member);
    let descriptor = zup_core::GenerationDescriptor::new(target.target, files)
        .map_err(|error| OperationError::refused("content", error))?;
    generation.sources.insert(
        path,
        zup_plan::GenerationSource {
            key,
            source_relative,
        },
    );
    generation.descriptor = descriptor;
    Ok(())
}

pub fn attach_preset_content(
    target: &mut zup_plan::BoundDeployment,
    preset: &zup_core::PresetRuntime,
    bytes: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(), OperationError> {
    use zup_core::{GenerationFile, MaintenanceArtifact};
    let executable = bytes(zup_bundle::PRESET_SOURCE).map_err(|error| {
        OperationError::refused(
            "content",
            format!("preset executable could not be read: {error}"),
        )
    })?;
    let executable_digest = zup_core::hash_bytes(&executable);
    let mut members = vec![(
        MaintenanceArtifact::Preset,
        GenerationFile {
            path: MaintenanceArtifact::Preset.member_path(target.target, executable_digest),
            size: executable.len() as u64,
            sha256: executable_digest,
            executable: true,
        },
        zup_core::RelativePath::new(zup_bundle::PRESET_SOURCE).expect("reserved source name"),
    )];
    for asset in &preset.assets {
        let source = zup_bundle::asset_source_name(asset.name.as_str());
        let content =
            bytes(source.as_str()).map_err(|error| OperationError::refused("content", error))?;
        if content.len() as u64 != asset.size || zup_core::hash_bytes(&content) != asset.sha256 {
            return Err(OperationError::refused(
                "content",
                format!(
                    "preset asset `{}` differs from the authenticated declaration",
                    asset.name
                ),
            ));
        }
        let artifact = MaintenanceArtifact::Asset {
            name: zup_core::RelativePath::new(asset.name.as_str())
                .map_err(|error| OperationError::refused("content", error))?,
        };
        members.push((
            artifact.clone(),
            GenerationFile {
                path: artifact.member_path(target.target, asset.sha256),
                size: asset.size,
                sha256: asset.sha256,
                executable: false,
            },
            source,
        ));
    }
    for (artifact, member, source) in members {
        attach_maintenance_member(target, artifact, member, source)?;
    }
    target.preset = Some(zup_core::InstalledPreset {
        preset: preset.clone(),
        executable: executable_digest,
    });
    target.requirements.preset = target.preset.clone();
    Ok(())
}

pub fn runtime_request(
    prepared: &PreparedOperation,
    state_root: PathBuf,
    options: RequestOptions,
) -> crate::RuntimeRequest {
    crate::RuntimeRequest {
        operation: crate::RuntimeOperation::Deployment(Box::new(crate::RuntimeDeployment {
            plan: prepared.transaction_plan.clone(),
            intent: zup_plan::DeploymentIntent {
                deployment: zup_plan::DeploymentSelection::from_plan(&prepared.transaction_plan),
                action: prepared.action,
                enable: prepared.selection.enable.clone(),
                disable: prepared.selection.disable.clone(),
                install_directory: prepared.selection.install_directory.clone(),
            },
            bootstrap: options.bootstrap,
            release: match &prepared.body {
                OperationBody::Retained { release, .. } | OperationBody::Cleanup { release } => {
                    release.clone()
                }
                _ => options.release,
            },
        })),
        target: prepared
            .target_plan()
            .map(|target| target.target)
            .unwrap_or_else(|| prepared.transaction_plan.target),
        app_id: prepared.transaction_plan.requirements.app.id.clone(),
        app_version: prepared.transaction_plan.requirements.app.version.clone(),
        scope: prepared.transaction_plan.requirements.scope,
        state_root,
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub bootstrap: Option<BootstrapRequest>,
    pub release: Option<zup_core::ReleaseIdentity>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use zup_core::Target;

    fn version(text: &str) -> semver::Version {
        semver::Version::parse(text).expect("valid version")
    }

    #[rstest]
    #[case(Operation::Install, None, "1.0.0", LifecycleAction::Install)]
    #[case(Operation::Apply, None, "1.0.0", LifecycleAction::Install)]
    #[case(Operation::Apply, Some("1.0.0"), "2.0.0", LifecycleAction::Upgrade)]
    #[case(Operation::Apply, Some("2.0.0"), "2.0.0", LifecycleAction::Modify)]
    #[case(Operation::Upgrade, Some("2.0.0"), "2.0.0", LifecycleAction::Upgrade)]
    #[case(Operation::Repair { force: true }, Some("1.0.0"), "1.0.0", LifecycleAction::Repair { force: true })]
    #[case(
        Operation::Uninstall,
        Some("1.0.0"),
        "1.0.0",
        LifecycleAction::Uninstall
    )]
    fn operations_resolve_to_the_lifecycle(
        #[case] operation: Operation,
        #[case] installed: Option<&str>,
        #[case] package: &str,
        #[case] expected: LifecycleAction,
    ) {
        let installed = installed.map(version);
        let action = resolve_operation(operation, installed.as_ref(), &version(package))
            .expect("a supported transition resolves");
        assert_eq!(action, expected);
    }

    #[test]
    fn apply_refuses_a_downgrade() {
        let installed = version("3.0.0");
        let error = resolve_operation(Operation::Apply, Some(&installed), &version("2.0.0"))
            .expect_err("a downgrade is refused");
        assert!(error.to_string().contains("downgrade"), "{error}");
    }

    #[test]
    fn a_template_is_not_a_chosen_directory() {
        let error = install_directory_template(Path::new("/opt/${location.user_data}"))
            .expect_err("template");
        assert!(error.to_string().contains("template variables"), "{error}");
    }

    struct StubHost {
        target: zup_plan::BoundDeployment,
        plan_calls: std::cell::Cell<usize>,
    }

    impl StubHost {
        fn installing(target: zup_plan::BoundDeployment) -> Self {
            Self {
                target,
                plan_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl OperationHost for StubHost {
        fn plan_build(
            &self,
            _build: &TargetBuildPlan,
            _request: &PlanRequest,
            _cancellation: &dyn CancellationQuery,
        ) -> Result<PlannedInstallation, OperationError> {
            self.plan_calls.set(self.plan_calls.get() + 1);
            Err(OperationError::refused("plan", "no planner in this test"))
        }

        fn resolve_target(
            &self,
            _install: &DesiredState,
        ) -> Result<zup_plan::BoundDeployment, String> {
            Err("no resolver in this test".into())
        }

        fn observe(
            &self,
            _target: Option<&zup_plan::BoundDeployment>,
            _ledger: Option<&InstallationRecord>,
        ) -> Result<(Option<HostSnapshot>, BTreeMap<zup_core::ResourceKey, bool>), String> {
            Ok((Some(HostSnapshot::default()), BTreeMap::new()))
        }

        fn compile(
            &self,
            execution: &ExecutionPlan,
            _target: Option<&zup_plan::BoundDeployment>,
            _ledger: Option<&InstallationRecord>,
        ) -> Result<DeploymentEffects, String> {
            let mut input = DeploymentEffects::new(
                self.target.installation,
                _ledger.map_or_else(
                    || self.target.requirements.clone(),
                    |record| record.current.requirements.clone(),
                ),
            );
            input.install_directory = execution.install_directory.clone();
            input.observed = _ledger.map(|record| record.current.selection());
            input.observed_transaction = _ledger.map(|record| record.commit.transaction);
            input.transition = execution.transition;
            Ok(input)
        }

        fn attach_content(
            &self,
            _target: &mut zup_plan::BoundDeployment,
            _content: &ContentAccess,
            _preset: Option<&zup_core::PresetRuntime>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn fixture_target() -> zup_plan::BoundDeployment {
        let target = Target::parse("windows-x64").expect("valid target");
        zup_plan::DesiredState {
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
            generation: None,
            app: zup_core::App {
                id: zup_core::AppId::new("com.example.stub").expect("valid id"),
                name: zup_core::NonEmptyString::new("Stub").expect("valid name"),
                version: semver::Version::new(1, 0, 0),
                publisher: None,
                main: None,
                description: None,
            },
            target,
            scope: SelectedScope::User,
            install_directory: zup_core::TargetPath::new(target, "C:\\Stub").expect("valid path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),

            preset: None,
        }
        .bind_fixed()
        .expect("fixed deployment requirements")
    }

    fn fixture_build() -> TargetBuildPlan {
        TargetBuildPlan {
            installer: zup_core::Installer {
                preset: None,
                app: zup_core::App {
                    id: zup_core::AppId::new("com.example.stub").expect("valid id"),
                    name: zup_core::NonEmptyString::new("Stub").expect("valid name"),
                    version: semver::Version::new(1, 0, 0),
                    publisher: None,
                    main: None,
                    description: None,
                },
                target: Target::parse("windows-x64").expect("valid target"),
                frontend: zup_core::Frontend::Console,
                updates: None,
                install: zup_core::Install {
                    generation_readers: Default::default(),
                    data: Vec::new(),
                    scope: zup_core::InstallScope::User,
                    directory: zup_core::InstallDirectory {
                        user: None,
                        machine: None,
                    },
                    allow_directory_override: false,
                },
                prerequisites: Vec::new(),
                components: Vec::new(),
                component_groups: Vec::new(),
                plugins: Vec::new(),
                files: Vec::new(),
                launchers: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_associations: Vec::new(),
            },
            prerequisites: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            ui_assets: Vec::new(),
            total_size: 0,
            prerequisite_size: 0,
            icons: zup_core::TargetIcons::default(),
        }
    }

    fn fixture_ledger(build: &TargetBuildPlan) -> InstallationRecord {
        let target = build.installer.target;
        {
            let fixture_commit = zup_plan::InstallationCommit {
                transaction: zup_core::TransactionId::new_v7(),
                plan_hash: zup_core::hash_bytes(b"runtime fixture"),
            };
            InstallationRecord {
                commit: fixture_commit,
                retired_deployments: Vec::new(),
                data: Default::default(),
                id: fixture_target().installation,
                state: zup_plan::InstallationState::Installed,
                app_id: build.installer.app.id.clone(),
                scope: SelectedScope::User,
                target,
                directory: zup_core::TargetPath::new(
                    target,
                    if target.operating_system() == zup_core::TargetOperatingSystem::Windows {
                        r"C:\Stub"
                    } else {
                        "/opt/stub"
                    },
                )
                .unwrap(),
                current: zup_plan::DeploymentSnapshot {
                    maintenance: None,
                    requirements: crate::requirements_fixture::requirements(
                        target,
                        SelectedScope::User,
                        build.installer.app.id.as_str(),
                        &build.installer.app.version.to_string(),
                    ),
                    revision: zup_core::ActivationRevision::FIRST,
                    transaction: fixture_commit.transaction,
                    plan_hash: fixture_commit.plan_hash,
                    version: build.installer.app.version.clone(),
                    selected_components: vec![],
                    release: None,
                    preset: None,
                    materialization: None,
                    resources: Default::default(),
                },
                retained: vec![],
            }
        }
    }

    #[rstest::rstest]
    #[case(b"first build", true)]
    #[case(b"second build", false)]
    fn maintenance_reuse_requires_the_complete_verified_content(
        #[case] requested: &[u8],
        #[case] reused: bool,
    ) {
        let root = tempfile::tempdir().unwrap();
        let installer = root.path().join("setup");
        let mut first = fixture_target();
        std::fs::write(&installer, b"first build").unwrap();
        let content = ContentAccess {
            installer: &installer,
            state_root: root.path(),
            scope: SelectedScope::User,
            action: LifecycleAction::Install,
            prior: None,
            maintenance: None,
        };
        attach_maintenance_copy(&mut first, &content, "runtime-source").unwrap();
        let original = first.maintenance.unwrap();
        let mut previous = fixture_ledger(&fixture_build());
        previous.current.maintenance = Some(
            zup_core::MaterializationSpec::new(
                original.installation,
                original.id,
                original.directory,
                original.descriptor,
            )
            .unwrap(),
        );
        previous.validate().unwrap();
        let mut candidate = fixture_target();
        std::fs::write(&installer, requested).unwrap();
        attach_maintenance_copy(&mut candidate, &content, "runtime-source").unwrap();
        reuse_unchanged_maintenance(&mut candidate, Some(&previous));
        let generation = candidate.maintenance.unwrap();
        assert_eq!(
            generation.id == previous.current.maintenance.as_ref().unwrap().id(),
            reused
        );
        assert_eq!(
            generation.preparation == zup_plan::GenerationPreparation::ReuseVerified,
            reused
        );
        assert_eq!(candidate.app.version, previous.current.version);
    }

    #[test]
    fn maintenance_repair_verifies_the_installed_instance_without_substituting_the_invoking_setup()
    {
        let root = tempfile::tempdir().unwrap();
        let installer = root.path().join("setup");
        std::fs::write(&installer, b"installed build").unwrap();
        let mut target = fixture_target();
        let mut content = ContentAccess {
            installer: &installer,
            state_root: root.path(),
            scope: SelectedScope::User,
            action: LifecycleAction::Install,
            prior: None,
            maintenance: None,
        };
        attach_maintenance_copy(&mut target, &content, "runtime-source").unwrap();
        let original = target.maintenance.take().unwrap();
        let mut previous = fixture_ledger(&fixture_build());
        previous.current.maintenance = Some(
            zup_core::MaterializationSpec::new(
                original.installation,
                original.id,
                original.directory,
                original.descriptor,
            )
            .unwrap(),
        );
        previous.validate().unwrap();
        std::fs::write(&installer, b"different invoking setup").unwrap();
        content.action = LifecycleAction::Repair { force: false };
        content.prior = Some(&previous);
        assert_eq!(
            attach_maintenance_copy(&mut target, &content, "runtime-source").unwrap(),
            zup_plan::GenerationPreparation::ReuseVerified
        );
        let reused = target.maintenance.unwrap();
        assert_eq!(
            reused.id,
            previous.current.maintenance.as_ref().unwrap().id()
        );
        assert_eq!(
            &reused.descriptor,
            previous.current.maintenance.as_ref().unwrap().descriptor()
        );
        assert!(reused.sources.is_empty());
        let wrong = zup_plan::GenerationSelection {
            materialization: zup_core::MaterializationId::new_v7(),
            policy: zup_plan::GenerationPolicy::ReuseVerified,
        };
        content.maintenance = Some(&wrong);
        assert!(
            attach_maintenance_copy(&mut fixture_target(), &content, "runtime-source").is_err()
        );
    }

    fn input<'a>(
        build: &'a TargetBuildPlan,
        installer: &'a Path,
        state_root: &'a Path,
        operation: Operation,
        prior: Option<&'a InstallationRecord>,
    ) -> OperationInput<'a> {
        OperationInput {
            build,
            installer,
            scope: SelectedScope::User,
            operation,
            selection: OperationSelection::default(),
            prior,
            state_root,
            cancellation: &zup_plan::NeverCancelled,
        }
    }

    #[rstest]
    #[case(Target::WINDOWS_X64)]
    #[case(Target::LINUX_X64)]
    fn an_uninstall_removes_by_ownership_without_planning(#[case] target: Target) {
        let mut build = fixture_build();
        build.installer.target = target;
        let mut ledger = fixture_ledger(&build);
        ledger
            .current
            .selected_components
            .push(zup_core::ComponentId::new("retired-component").unwrap());
        let root = tempfile::tempdir().expect("temp dir");
        let installer = root.path().join("setup.exe");
        std::fs::write(&installer, b"installer").expect("write");
        let mut host_target = fixture_target();
        host_target.target = target;
        host_target.install_directory = ledger.directory.clone();
        let host = StubHost::installing(host_target);
        let prepared = prepare_operation(
            &input(
                &build,
                &installer,
                root.path(),
                Operation::Uninstall,
                Some(&ledger),
            ),
            &host,
        )
        .expect("an uninstall prepares");
        assert_eq!(prepared.action, LifecycleAction::Uninstall);
        assert_eq!(host.plan_calls.get(), 0);
        assert!(prepared.install_plan().is_none());
        assert!(prepared.target_plan().is_none());
        assert!(prepared.transaction_plan.transition.is_remove());
    }

    #[test]
    fn a_repair_refuses_a_selection_it_does_not_use() {
        let build = fixture_build();
        let ledger = fixture_ledger(&build);
        let root = tempfile::tempdir().expect("temp dir");
        let installer = root.path().join("setup.exe");
        std::fs::write(&installer, b"installer").expect("write");
        let host = StubHost::installing(fixture_target());
        let mut selection = OperationSelection::default();
        selection.enable.push("docs".to_owned());
        let input = OperationInput {
            selection,
            ..input(
                &build,
                &installer,
                root.path(),
                Operation::Repair { force: false },
                Some(&ledger),
            )
        };
        let error = prepare_operation(&input, &host).expect_err("a repair takes no selection");
        assert!(
            error.to_string().contains("committed component selection"),
            "{error}"
        );
        assert_eq!(host.plan_calls.get(), 0);
    }

    #[test]
    fn an_install_without_a_ledger_prepares_without_prior_selection() {
        let build = fixture_build();
        let root = tempfile::tempdir().expect("temp dir");
        let installer = root.path().join("setup.exe");
        std::fs::write(&installer, b"installer").expect("write");
        let host = StubHost::installing(fixture_target());
        let error = prepare_operation(
            &input(&build, &installer, root.path(), Operation::Install, None),
            &host,
        )
        .expect_err("the stub plans nothing");
        assert!(error.to_string().contains("plan"), "{error}");
        assert_eq!(host.plan_calls.get(), 1);
    }
    #[test]
    fn cancellation_before_planning_never_calls_the_host() {
        let build = fixture_build();
        let root = tempfile::tempdir().unwrap();
        let host = StubHost::installing(fixture_target());
        let mut request = input(
            &build,
            Path::new("unused"),
            root.path(),
            Operation::Install,
            None,
        );
        request.cancellation = &|| true;
        assert!(matches!(
            prepare_operation(&request, &host),
            Err(OperationError::Cancelled)
        ));
        assert_eq!(host.plan_calls.get(), 0);
    }
}
