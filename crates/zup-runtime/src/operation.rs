use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{ComponentId, InstallScope, ResourceKey, SelectedScope, TargetBuildPlan, Template};
use zup_exec::{ExecutionPlan, HostSnapshot, InstallLedger, LifecycleAction, plan_lifecycle};
use zup_plan::{CancellationQuery, InstallPlan, PlanRequest, PlannedInstallation};
use zup_platform::TargetPlan;
use zup_transaction::{TransactionInput, TransactionPlan, compile_transaction};

use crate::BootstrapRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Install,
    Upgrade,
    Modify,
    Repair { force_files: bool },
    Uninstall,
    Apply,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
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
            LifecycleAction::Install => Self::Install,
            LifecycleAction::Upgrade => Self::Upgrade,
            LifecycleAction::Modify => Self::Modify,
            LifecycleAction::Repair { force_files } => Self::Repair { force_files },
            LifecycleAction::Uninstall => Self::Uninstall,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct OperationSelection {
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationError {
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
    fn refused(stage: &'static str, message: impl std::fmt::Display) -> Self {
        Self::Refused {
            stage,
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Downgrade {
                installed,
                requested,
            } => write!(out, "downgrade from {installed} to {requested} is refused"),
            Self::Refused { stage, message } => write!(out, "{stage}: {message}"),
        }
    }
}

impl std::error::Error for OperationError {}

pub fn resolve_operation(
    operation: Operation,
    installed: Option<&semver::Version>,
    package: &semver::Version,
) -> Result<LifecycleAction, OperationError> {
    match operation {
        Operation::Install => Ok(LifecycleAction::Install),
        Operation::Upgrade => Ok(LifecycleAction::Upgrade),
        Operation::Modify => Ok(LifecycleAction::Modify),
        Operation::Repair { force_files } => Ok(LifecycleAction::Repair { force_files }),
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

pub fn resolve_scope(
    declared: InstallScope,
    requested: Option<SelectedScope>,
) -> Result<SelectedScope, OperationError> {
    match (declared, requested) {
        (_, Some(scope)) if declares(declared, scope) => Ok(scope),
        (_, Some(_)) => Err(OperationError::refused(
            "scope",
            format!(
                "this installer does not serve the requested scope (manifest scope: {declared})"
            ),
        )),
        (InstallScope::User, None) => Ok(SelectedScope::User),
        (InstallScope::Machine, None) => Ok(SelectedScope::Machine),
        (InstallScope::Either, None) => Err(OperationError::refused(
            "scope",
            "this installer serves the current user or the whole machine: choose an explicit scope",
        )),
    }
}

fn declares(declared: InstallScope, scope: SelectedScope) -> bool {
    match scope {
        SelectedScope::User => declared.allows_user(),
        SelectedScope::Machine => declared.allows_machine(),
    }
}

pub struct OperationInput<'a> {
    pub build: &'a TargetBuildPlan,
    pub installer: &'a Path,
    pub scope: SelectedScope,
    pub operation: Operation,
    pub selection: OperationSelection,
    pub prior: Option<&'a InstallLedger>,
    pub state_root: &'a Path,
    pub cancellation: &'a dyn CancellationQuery,
}

pub struct ContentAccess<'a> {
    pub installer: &'a Path,
    pub state_root: &'a Path,
    pub scope: SelectedScope,
}

pub trait OperationHost {
    fn check_capabilities(&self, _input: &OperationInput) -> Result<(), String> {
        Ok(())
    }

    fn plan_build(
        &self,
        build: &TargetBuildPlan,
        request: &PlanRequest,
        _cancellation: &dyn CancellationQuery,
    ) -> Result<PlannedInstallation, String> {
        let planning = zup_core::BuildPlan {
            targets: vec![build.clone()],
        };
        zup_plan::plan(&planning, request)
            .map(|plan| PlannedInstallation {
                plan,
                generated_files: Vec::new(),
            })
            .map_err(|error| error.to_string())
    }

    fn resolve_target(&self, install: &InstallPlan) -> Result<TargetPlan, String>;

    fn resolve_target_for_uninstall(&self) -> bool {
        false
    }

    fn observe(
        &self,
        target: Option<&TargetPlan>,
        ledger: Option<&InstallLedger>,
    ) -> Result<(Option<HostSnapshot>, BTreeMap<ResourceKey, bool>), String>;

    fn compile(
        &self,
        action: LifecycleAction,
        execution: &ExecutionPlan,
        target: Option<&TargetPlan>,
        ledger: Option<&InstallLedger>,
    ) -> Result<TransactionInput, String>;

    fn attach_content(
        &self,
        target: &mut TargetPlan,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String>;

    fn install_directory(
        &self,
        explicit: Option<&Path>,
        prior: Option<&InstallLedger>,
        allowed: bool,
        repair: bool,
    ) -> Result<Option<Template>, String> {
        choose_install_directory(explicit, prior, allowed, repair)
            .map_err(|error| error.to_string())
    }

    fn prepare_bootstrap(
        &self,
        _install: &InstallPlan,
    ) -> Result<Option<BootstrapRequest>, String> {
        Ok(None)
    }
}

#[derive(Debug)]
pub struct PlannedOperation {
    pub install_plan: InstallPlan,
    pub target_plan: TargetPlan,
    pub generated_files: Vec<zup_plan::GeneratedFile>,
    pub bootstrap: Option<BootstrapRequest>,
}

#[derive(Debug)]
pub enum OperationBody {
    Planned(Box<PlannedOperation>),
    Uninstall {
        target_plan: Option<Box<TargetPlan>>,
    },
}

#[derive(Debug)]
pub struct PreparedOperation {
    pub operation: Operation,
    pub action: LifecycleAction,
    pub execution_plan: ExecutionPlan,
    pub transaction_plan: TransactionPlan,
    pub selection: OperationSelection,
    pub body: OperationBody,
}

impl PreparedOperation {
    pub fn target_plan(&self) -> Option<&TargetPlan> {
        match &self.body {
            OperationBody::Planned(planned) => Some(&planned.target_plan),
            OperationBody::Uninstall { target_plan } => target_plan.as_deref(),
        }
    }

    pub fn install_plan(&self) -> Option<&InstallPlan> {
        match &self.body {
            OperationBody::Planned(planned) => Some(&planned.install_plan),
            OperationBody::Uninstall { .. } => None,
        }
    }

    pub fn generated_files(&self) -> &[zup_plan::GeneratedFile] {
        match &self.body {
            OperationBody::Planned(planned) => &planned.generated_files,
            OperationBody::Uninstall { .. } => &[],
        }
    }

    pub fn bootstrap(&self) -> Option<&BootstrapRequest> {
        match &self.body {
            OperationBody::Planned(planned) => planned.bootstrap.as_ref(),
            OperationBody::Uninstall { .. } => None,
        }
    }
}

pub fn prepare_operation<H: OperationHost>(
    input: &OperationInput,
    host: &H,
) -> Result<PreparedOperation, OperationError> {
    host.check_capabilities(input)
        .map_err(|message| OperationError::refused("capabilities", message))?;
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
        input.prior.as_ref().map(|ledger| &ledger.version),
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
        let target = if host.resolve_target_for_uninstall() {
            let plan_request = PlanRequest::new(installer.target, input.scope);
            let planned = host
                .plan_build(input.build, &plan_request, input.cancellation)
                .map_err(|message| OperationError::refused("plan", message))?;
            let mut target = host
                .resolve_target(&planned.plan)
                .map_err(|message| OperationError::refused("target", message))?;
            host.attach_content(
                &mut target,
                &ContentAccess {
                    installer: input.installer,
                    state_root: input.state_root,
                    scope: input.scope,
                },
                None,
            )
            .map_err(|message| OperationError::refused("content", message))?;
            Some(target)
        } else {
            None
        };
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
        let transaction_input = host
            .compile(action, &execution, target.as_ref(), Some(ledger))
            .map_err(|message| OperationError::refused("transaction", message))?;
        let transaction_plan = compile_transaction(&transaction_input)
            .map_err(|error| OperationError::refused("transaction", error))?;
        return Ok(PreparedOperation {
            operation: input.operation,
            action,
            execution_plan: execution,
            transaction_plan,
            selection: input.selection.clone(),
            body: OperationBody::Uninstall {
                target_plan: target.map(Box::new),
            },
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
    let plan_request = plan_request(
        installer,
        input.scope,
        input.prior,
        &input.selection,
        action,
        install_directory,
    )?;
    let planned = host
        .plan_build(input.build, &plan_request, input.cancellation)
        .map_err(|message| OperationError::refused("plan", message))?;
    let mut target = host
        .resolve_target(&planned.plan)
        .map_err(|message| OperationError::refused("target", message))?;
    host.attach_content(
        &mut target,
        &ContentAccess {
            installer: input.installer,
            state_root: input.state_root,
            scope: input.scope,
        },
        input.build.installer.preset.as_ref(),
    )
    .map_err(|message| OperationError::refused("content", message))?;
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
    let transaction_input = host
        .compile(action, &execution, Some(&target), input.prior)
        .map_err(|message| OperationError::refused("transaction", message))?;
    let transaction_plan = compile_transaction(&transaction_input)
        .map_err(|error| OperationError::refused("transaction", error))?;
    let bootstrap = host
        .prepare_bootstrap(&planned.plan)
        .map_err(|message| OperationError::refused("prerequisites", message))?;
    Ok(PreparedOperation {
        operation: input.operation,
        action,
        execution_plan: execution,
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

pub fn choose_install_directory(
    explicit: Option<&Path>,
    prior: Option<&InstallLedger>,
    allowed: bool,
    repair: bool,
) -> Result<Option<Template>, OperationError> {
    if repair {
        if !allowed {
            return Ok(None);
        }
        return Ok(persisted_install_directory(prior));
    }
    choose_explicit_directory(explicit, prior, allowed)
}

fn choose_explicit_directory(
    explicit: Option<&Path>,
    prior: Option<&InstallLedger>,
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
    if !allowed {
        return Ok(None);
    }
    Ok(prior
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| Template::parse(&path.to_string()).ok()))
}

pub fn persisted_install_directory(prior: Option<&InstallLedger>) -> Option<Template> {
    prior
        .and_then(|ledger| ledger.install_directory.as_ref())
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
    prior: Option<&InstallLedger>,
    selection: &OperationSelection,
    action: LifecycleAction,
    install_directory: Option<Template>,
) -> Result<PlanRequest, OperationError> {
    let mut request = PlanRequest::new(installer.target, scope);
    request.install_directory = install_directory;
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous =
            prior.ok_or_else(|| OperationError::refused("resolve", "installation not found"))?;
        for component in &installer.components {
            if previous.selected_components.contains(&component.id) {
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

pub fn attach_maintenance_copy(
    target: &mut TargetPlan,
    installer: &Path,
    state_root: &Path,
    scope: SelectedScope,
    source_name: &str,
) -> Result<(), OperationError> {
    let file = std::fs::File::open(installer).map_err(|error| {
        OperationError::refused("content", format!("installer executable: {error}"))
    })?;
    let (size, sha256) = zup_core::hash_reader(file).map_err(|error| {
        OperationError::refused("content", format!("installer executable: {error}"))
    })?;
    let destination = zup_platform::TargetPath::new(
        target.target,
        zup_transaction::maintenance_runtime_path(
            state_root,
            &target.app.id,
            scope,
            &target.app.version,
            target.target.executable_suffix(),
        )
        .to_string_lossy(),
    )
    .map_err(|error| OperationError::refused("content", error))?;
    target.files.push(zup_platform::TargetFile {
        key: zup_core::ResourceKey::Maintenance {
            app_id: target.app.id.to_string(),
            version: target.app.version.to_string(),
            destination: destination.to_string(),
        },
        source_relative: zup_core::RelativePath::new(source_name)
            .map_err(|error| OperationError::refused("content", error))?,
        destination,
        size,
        sha256,
        privilege: scope.authorization(),
        executable: true,
    });
    target.summary.file_count += 1;
    target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    target.summary.resource_count += 1;
    Ok(())
}

pub fn runtime_request(
    prepared: &PreparedOperation,
    app_id: zup_core::AppId,
    app_version: semver::Version,
    scope: SelectedScope,
    state_root: PathBuf,
    options: RequestOptions,
) -> crate::RuntimeRequest {
    crate::RuntimeRequest {
        target: prepared
            .target_plan()
            .map(|target| target.target)
            .unwrap_or_else(|| prepared.transaction_plan.target),
        app_id,
        app_version,
        scope,
        transaction_plan: prepared.transaction_plan.clone(),
        state_root: state_root.clone(),
        work_root: state_root.join("work"),
        recovery_id: options.recovery_id,
        bootstrap: options.bootstrap,
        release: options.release,
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub bootstrap: Option<BootstrapRequest>,
    pub release: Option<zup_core::ReleaseIdentity>,
    pub recovery_id: Option<zup_transaction::TransactionId>,
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
    #[case(Operation::Repair { force_files: true }, Some("1.0.0"), "1.0.0", LifecycleAction::Repair { force_files: true })]
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

    #[rstest]
    #[case(InstallScope::User, None, SelectedScope::User)]
    #[case(InstallScope::Machine, None, SelectedScope::Machine)]
    #[case(
        InstallScope::Either,
        Some(SelectedScope::Machine),
        SelectedScope::Machine
    )]
    #[case(InstallScope::User, Some(SelectedScope::User), SelectedScope::User)]
    fn scope_resolution_is_explicit(
        #[case] declared: InstallScope,
        #[case] requested: Option<SelectedScope>,
        #[case] expected: SelectedScope,
    ) {
        assert_eq!(resolve_scope(declared, requested).expect("scope"), expected);
    }

    #[test]
    fn an_either_installer_without_a_choice_is_refused() {
        assert!(resolve_scope(InstallScope::Either, None).is_err());
        assert!(resolve_scope(InstallScope::User, Some(SelectedScope::Machine)).is_err());
    }

    #[test]
    fn a_template_is_not_a_chosen_directory() {
        let error = install_directory_template(Path::new("/opt/${location.user_data}"))
            .expect_err("template");
        assert!(error.to_string().contains("template variables"), "{error}");
    }

    struct StubHost {
        target: zup_platform::TargetPlan,
        plan_calls: std::cell::Cell<usize>,
    }

    impl StubHost {
        fn installing(target: zup_platform::TargetPlan) -> Self {
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
        ) -> Result<PlannedInstallation, String> {
            self.plan_calls.set(self.plan_calls.get() + 1);
            Err("no planner in this test".into())
        }

        fn resolve_target(&self, _install: &InstallPlan) -> Result<TargetPlan, String> {
            Err("no resolver in this test".into())
        }

        fn observe(
            &self,
            _target: Option<&TargetPlan>,
            _ledger: Option<&InstallLedger>,
        ) -> Result<(Option<HostSnapshot>, BTreeMap<zup_core::ResourceKey, bool>), String> {
            Ok((Some(HostSnapshot::default()), BTreeMap::new()))
        }

        fn compile(
            &self,
            _action: LifecycleAction,
            execution: &ExecutionPlan,
            _target: Option<&TargetPlan>,
            _ledger: Option<&InstallLedger>,
        ) -> Result<TransactionInput, String> {
            let mut input = TransactionInput::new(self.target.target);
            input.uninstall = execution.uninstall;
            Ok(input)
        }

        fn attach_content(
            &self,
            _target: &mut TargetPlan,
            _content: &ContentAccess,
            _preset: Option<&zup_core::PresetRuntime>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn fixture_target() -> zup_platform::TargetPlan {
        let target = Target::parse("windows-x64").expect("valid target");
        zup_platform::TargetPlan {
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
            install_directory: zup_platform::TargetPath::new(target, "C:\\Stub")
                .expect("valid path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_platform::TargetPlanSummary {
                file_count: 0,
                install_bytes: 0,
                resource_count: 0,
                requires_authorization: false,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
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

    fn fixture_ledger(build: &TargetBuildPlan) -> InstallLedger {
        let mut ledger = InstallLedger::new(
            build.installer.app.id.clone(),
            build.installer.target,
            SelectedScope::User,
        );
        ledger.version = build.installer.app.version.clone();
        ledger
    }

    fn input<'a>(
        build: &'a TargetBuildPlan,
        installer: &'a Path,
        state_root: &'a Path,
        operation: Operation,
        prior: Option<&'a InstallLedger>,
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

    #[test]
    fn an_uninstall_removes_by_ownership_without_planning() {
        let build = fixture_build();
        let ledger = fixture_ledger(&build);
        let root = tempfile::tempdir().expect("temp dir");
        let installer = root.path().join("setup.exe");
        std::fs::write(&installer, b"installer").expect("write");
        let host = StubHost::installing(fixture_target());
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
        assert!(prepared.transaction_plan.uninstall);
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
                Operation::Repair { force_files: false },
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
}
