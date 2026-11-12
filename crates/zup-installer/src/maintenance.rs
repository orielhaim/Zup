use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use zup_core::TransactionId;

use clap::{Args, Subcommand, ValueHint};
use zup_acquire::CachePolicy;
use zup_core::{AppId, Frontend, SelectedScope};
use zup_plan::LifecycleAction;
use zup_presentation::{InstallerEvent, InstallerResult, OutputFormat, ProcessOutcome};
use zup_runtime::{ExecutionPolicy, RuntimeRequest};
use zup_transaction::TransactionStore;
use zup_update::{ComponentSelection, TrustContext};

use crate::lifecycle::{self, PreparedRuntime};
use crate::package;
use crate::run::RuntimeContext;
use crate::terminal::{Output, Scope};
use crate::{acquire, execute};

#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: Output,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub plain: bool,

    #[arg(long, hide = true)]
    pub app_id: Option<String>,
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub ui: bool,
}

impl UninstallArgs {
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }
}

pub fn run_uninstall(context: RuntimeContext, args: UninstallArgs) -> miette::Result<()> {
    let executable = package::current_executable()?;

    if context.frontend != Frontend::Gui && package::open_bundle_if_present(&executable)?.is_some()
    {
        let scope = args.scope.map(Into::into);
        return crate::terminal::uninstall(
            crate::host::WindowsSessionHost::default(),
            context.with_output(args.output()),
            scope,
            args.state_root,
            args.non_interactive,
            args.yes,
            args.plain,
        );
    }

    if args.ui {
        let bundle = package::open_bundle(&executable)?;
        return crate::frontend::graphical_uninstall(context, &executable, &bundle, &args);
    }

    if context
        .with_output(args.output())
        .may_prompt(args.non_interactive, args.yes)
        && !crate::frontend::confirm_uninstall()?
    {
        return Err(crate::frontend::cancelled());
    }

    let bundle = package::open_bundle_if_present(&executable)?;
    let build = bundle.as_ref().map(package::target_plan).transpose()?;
    let app_id = match args.app_id.as_deref() {
        Some(id) => AppId::new(id).map_err(|error| miette::miette!("application ID: {error}"))?,
        None => build
            .as_ref()
            .ok_or_else(|| miette::miette!("this file carries no installer package"))?
            .installer
            .app
            .id
            .clone(),
    };
    let (scope, state_root) = resolve_scope(
        &app_id,
        build.as_ref().map(|build| &build.installer),
        args.scope.map(Into::into),
        args.state_root.as_deref(),
    )?;
    let output = args.output();
    let policy = if context
        .with_output(args.output())
        .may_prompt(args.non_interactive, args.yes)
    {
        ExecutionPolicy::Interactive
    } else {
        ExecutionPolicy::NonInteractive
    };

    let prepared = match &build {
        Some(_) => prepare_from_package(scope, state_root.clone())?,
        None => prepare_from_ledger(
            &app_id,
            scope,
            state_root.clone(),
            build.as_ref().map(|build| build.installer.frontend),
        )?,
    };
    let result = if output == OutputFormat::Human {
        crate::execute::execute_with_policy(prepared, policy).map_err(Into::into)
    } else {
        crate::execute::execute_frontend(prepared, output, LifecycleAction::Uninstall)
            .map_err(Into::into)
    };
    if result.is_ok() {
        zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
            .map_err(|error| miette::miette!("clean up payload overlays: {error}"))?;
    }
    result
}

fn prepare_from_package(
    scope: SelectedScope,
    state_root: PathBuf,
) -> miette::Result<PreparedRuntime> {
    lifecycle::prepare_embedded_transition_with_cancellation(
        zup_runtime::Operation::Uninstall,
        scope,
        Some(state_root),
        Vec::new(),
        Vec::new(),
        None,
        lifecycle::EmbeddedPreparationMode {
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: false,
        },
    )
}

/// tree because there was never one.
fn prepare_from_ledger(
    app_id: &AppId,
    scope: SelectedScope,
    state_root: PathBuf,
    frontend: Option<Frontend>,
) -> miette::Result<PreparedRuntime> {
    let ledger = zup_windows::InstallationRecordStore::new(&state_root)
        .observe(app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .filter(|record| record.is_installed())
        .ok_or_else(|| miette::miette!("installation not found"))?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        LifecycleAction::Uninstall,
        app_id,
        scope,
        None,
        &state_root,
        frontend.unwrap_or(Frontend::Gui),
    )
    .map_err(|error| miette::miette!("uninstall plan: {error}"))?;
    // never reads a payload file, because every file it removes is named by the
    let payload_root =
        std::env::current_dir().map_err(|error| miette::miette!("working directory: {error}"))?;
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: execution.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&execution),
                    action: zup_plan::LifecycleAction::Uninstall,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: ledger.target,
        app_id: app_id.clone(),
        app_version: ledger.current.version,
        scope,
        state_root: state_root.clone(),
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_path(payload_root, None)
        .map_err(|error| miette::miette!("payload source: {error}"))?;
    Ok(PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
        action: LifecycleAction::Uninstall,
    })
}

fn resolve_scope(
    app_id: &AppId,
    installer: Option<&zup_core::Installer>,
    requested: Option<SelectedScope>,
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    if let Some(installer) = installer {
        let scope =
            installer.install.scope.select(requested).map_err(|error| {
                miette::miette!("{error}; specify --scope user or --scope machine")
            })?;
        return Ok((
            scope,
            resolve_state_root(app_id, state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    if let Some(scope) = requested {
        return Ok((
            scope,
            resolve_state_root(app_id, state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    discover_scope(
        app_id,
        &[SelectedScope::User, SelectedScope::Machine],
        state_root,
    )
}

fn discover_scope(
    app_id: &AppId,
    allowed: &[SelectedScope],
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    let mut found = Vec::new();
    for scope in allowed.iter().copied() {
        let root = resolve_state_root(app_id, state_root.map(Path::to_path_buf), scope)?;
        let installed = zup_windows::InstallationRecordStore::new(&root)
            .observe(app_id, scope)
            .map_err(|error| miette::miette!("ledger: {error}"))?
            .is_some();
        if installed {
            found.push((scope, root));
        }
    }
    if found.len() > 1 {
        return Err(miette::miette!(
            "this application is installed in both scopes; choose --scope"
        ));
    }
    if let Some(result) = found.pop() {
        return Ok(result);
    }
    Err(miette::miette!(
        "this application is not installed; specify --scope to inspect one installation"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_wide_application_is_only_ever_uninstalled_from_the_machine_scope() {
        let app = AppId::new("com.example.acme").expect("valid");
        let installer = zup_core::Installer {
            preset: None,
            app: zup_core::App {
                id: app.clone(),
                name: zup_core::NonEmptyString::new("Acme").expect("valid"),
                version: semver::Version::parse("1.0.0").expect("valid"),
                publisher: None,
                main: None,
                description: None,
            },
            target: zup_core::Target::parse("windows-x64").expect("valid"),
            frontend: Frontend::Headless,
            updates: None,
            install: zup_core::Install {
                generation_readers: Default::default(),
                data: Vec::new(),
                scope: zup_core::InstallScope::Machine,
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
        };
        let (scope, root) = resolve_scope(&app, Some(&installer), None, None)
            .expect("a machine-wide application resolves without asking");
        assert!(
            root.ends_with(Path::new("zup").join("apps").join(app.state_key().to_hex())),
            "machine scope uses the application-specific authority root"
        );
        assert_eq!(scope, SelectedScope::Machine);
        assert!(resolve_scope(&app, Some(&installer), Some(SelectedScope::User), None).is_err());
    }
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: Option<UpdateCommand>,
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: Output,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub plain: bool,

    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    Check,
}

impl UpdateArgs {
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }
}

pub fn run_update(context: RuntimeContext, args: UpdateArgs) -> miette::Result<()> {
    let executable = package::current_executable()?;
    let bundle = package::open_bundle(&executable).map_err(|error| {
        miette::miette!("an update needs this application's installer package: {error}")
    })?;
    let build = package::target_plan(&bundle)?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| miette::miette!("this application is not configured for updates"))?;
    let output = args.output();
    let check_only = args.command.is_some();
    let machine_run = output != OutputFormat::Human && !check_only;

    let (scope, state_root, ledger) = committed_installation(context, installer, &args)?;
    let update_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        peer_user_state_root(&installer.app.id)?
    } else {
        state_root.clone()
    };

    if output == OutputFormat::Jsonl && !machine_run {
        println!(
            "{}",
            serde_json::to_string(&InstallerEvent::started(
                installer.app.id.as_str(),
                ledger.current.version.to_string(),
                "update",
            ))
            .map_err(|error| miette::miette!("output: {error}"))?
        );
        println!(
            "{}",
            serde_json::to_string(&InstallerEvent::Phase {
                state: "resolving".into(),
            })
            .map_err(|error| miette::miette!("output: {error}"))?
        );
    } else if output == OutputFormat::Human && std::io::stderr().is_terminal() {
        eprintln!("Checking for updates…");
    }

    let installed = ledger.release_identity().cloned();
    let sink = zup_acquire::ProgressSink::discard();
    let trust = TrustContext::from_update_config(
        config,
        installer.app.id.clone(),
        installer.install.scope,
        update_root.clone(),
    );
    let mut resolver = zup_update::ReleaseResolver::new(
        trust,
        zup_acquire::HostProfile::native(),
        CachePolicy::Auto,
        sink.clone(),
    )
    .map_err(|error| miette::miette!("update resolver: {error}"))?;
    if let Some(source) = &args.source {
        resolver = resolver.with_seed("local source", source.clone());
    }
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("update runtime: {error}"))?;
    let resolved = tokio
        .block_on(resolver.resolve())
        .map_err(|error| miette::miette!("update check: {error}"))?;

    let candidate = zup_core::ReleaseCandidate {
        version: resolved
            .descriptor
            .version
            .parse()
            .map_err(|error: semver::Error| miette::miette!("update version: {error}"))?,
        release: resolved.descriptor.release_digest,
    };
    if !candidate.is_update_for(&ledger.current.version, installed.as_ref()) {
        return up_to_date(
            output,
            installer.app.id.as_str(),
            ledger.current.version.to_string(),
            scope,
            machine_run,
        );
    }
    if check_only {
        return update_available(
            output,
            installer.app.id.as_str(),
            ledger.current.version.to_string(),
            resolved.descriptor.version.clone(),
            scope,
        );
    }

    confirm(context, &args)?;
    if output == OutputFormat::Human {
        println!(
            "update available: {} → {}",
            ledger.current.version, resolved.descriptor.version
        );
        if std::io::stderr().is_terminal() {
            eprintln!("Acquiring the update…");
        }
    }
    let acquired = tokio
        .block_on(acquire::acquire_closure(
            &resolver,
            resolved,
            ComponentSelection::All,
        ))
        .map_err(|error| miette::miette!("update acquisition: {error}"))?;
    if output == OutputFormat::Human {
        eprintln!("  {}", acquired.estimate());
    }
    let policy = if context.may_prompt(args.non_interactive, args.yes) {
        ExecutionPolicy::Interactive
    } else {
        ExecutionPolicy::NonInteractive
    };
    lifecycle::run_acquired_transition(
        acquire::Request {
            state_root: Some(state_root),
            scope: Some(scope),
            ..acquire::Request::new(LifecycleAction::Upgrade)
        },
        &acquired,
        output,
        policy,
    )
}

fn committed_installation(
    context: RuntimeContext,
    installer: &zup_core::Installer,
    args: &UpdateArgs,
) -> miette::Result<(SelectedScope, PathBuf, zup_plan::InstallationRecord)> {
    let scope = crate::terminal::choose_scope(
        installer,
        args.scope.map(Into::into),
        context.may_prompt(args.non_interactive, args.yes),
        args.plain,
    )?;
    let state_root = resolve_state_root(&installer.app.id, args.state_root.clone(), scope)?;
    let ledger = zup_windows::InstallationRecordStore::new(&state_root)
        .observe(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?
        .ok_or_else(|| {
            miette::miette!("this application is not installed in the selected scope")
        })?;
    Ok((scope, state_root, ledger))
}

fn confirm(context: RuntimeContext, args: &UpdateArgs) -> miette::Result<()> {
    if context.may_prompt(args.non_interactive, args.yes)
        && !crate::frontend::confirm_update(args.plain)?
    {
        return Err(crate::frontend::cancelled());
    }
    Ok(())
}

fn up_to_date(
    output: OutputFormat,
    application: &str,
    version: String,
    scope: SelectedScope,
    machine_run: bool,
) -> miette::Result<()> {
    match output {
        OutputFormat::Human => println!("up to date ({version})"),
        OutputFormat::Json => {
            let mut result = InstallerResult::new(ProcessOutcome::Success, application, version);
            result.scope = Some(scope);
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
        OutputFormat::Jsonl => {
            if machine_run {
                println!(
                    "{}",
                    serde_json::to_string(&InstallerEvent::started(application, version, "update"))
                        .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
            println!(
                "{}",
                serde_json::to_string(&InstallerEvent::Completed {
                    outcome: ProcessOutcome::Success,
                })
                .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
    }
    Ok(())
}

fn update_available(
    output: OutputFormat,
    application: &str,
    current: String,
    available: String,
    scope: SelectedScope,
) -> miette::Result<()> {
    match output {
        OutputFormat::Human => println!("update available: {current} → {available}"),
        OutputFormat::Json => {
            let mut result =
                InstallerResult::new(ProcessOutcome::Success, application, available.clone());
            result.scope = Some(scope);
            result.message = Some(format!("update available from {current}"));
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
        OutputFormat::Jsonl => {
            for state in ["available", "completed"] {
                let event = if state == "available" {
                    InstallerEvent::Phase {
                        state: state.into(),
                    }
                } else {
                    InstallerEvent::Completed {
                        outcome: ProcessOutcome::Success,
                    }
                };
                println!(
                    "{}",
                    serde_json::to_string(&event)
                        .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct RecoveryArgs {
    #[arg(long, conflicts_with = "decision")]
    pub assess: bool,
    #[arg(long, value_name = "JSON")]
    pub decision: Option<PathBuf>,
    #[arg(long = "transaction")]
    pub transaction_id: uuid::Uuid,
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: Output,

    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
}

pub fn run_recovery(context: RuntimeContext, args: RecoveryArgs) -> miette::Result<()> {
    let _ = context;
    let recovery_request = crate::recovery_request(args.assess, args.decision.as_deref())?;
    let executable = package::current_executable()?;
    let bundle = package::open_bundle_if_present(&executable)?;
    let embedded = bundle.as_ref().map(package::target_plan).transpose()?;
    let scope = match &embedded {
        Some(build) => build
            .installer
            .install
            .scope
            .select(args.scope.map(Into::into))
            .map_err(|error| miette::miette!("{error}; specify --scope user or --scope machine"))?,
        None => args
            .scope
            .map(Into::into)
            .ok_or_else(|| miette::miette!("recovery requires --scope user or --scope machine"))?,
    };
    let id = TransactionId::from_uuid(args.transaction_id);
    let state_root = match &embedded {
        Some(build) => resolve_state_root(&build.installer.app.id, args.state_root, scope)?,
        None if scope == SelectedScope::User => {
            let root = args.state_root.ok_or_else(|| {
                miette::miette!("recovery without an embedded application requires --state-root")
            })?;
            let root = std::path::absolute(root)
                .map_err(|error| miette::miette!("state root: {error}"))?;
            let record = zup_transaction::FilesystemTransactionStore::private_at(&root)
                .load(&id)
                .map_err(|error| miette::miette!("transaction: {error}"))?;
            resolve_state_root(&record.app_id, Some(root), scope)?
        }
        None => {
            return Err(miette::miette!(
                "machine recovery uses the package embedded in this installer"
            ));
        }
    };
    if scope == SelectedScope::Machine {
        let build = embedded.ok_or_else(|| {
            miette::miette!("machine recovery uses the package embedded in this installer")
        })?;
        let request = RuntimeRequest {
            operation: zup_runtime::RuntimeOperation::Recovery {
                transaction: id,
                request: recovery_request,
            },
            target: build.installer.target,
            app_id: build.installer.app.id.clone(),
            app_version: build.installer.app.version.clone(),
            scope,
            state_root,
        };
        let backend = zup_windows::WindowsRuntimeBackend::for_recovery();
        let prepared = PreparedRuntime {
            request,
            backend: std::sync::Arc::new(backend),
            action: LifecycleAction::Repair { force: false },
        };
        return if OutputFormat::from(args.output) == OutputFormat::Human {
            execute::execute(prepared)
        } else {
            execute::execute_frontend(
                prepared,
                OutputFormat::from(args.output),
                LifecycleAction::Repair { force: false },
            )
            .map_err(Into::into)
        };
    }
    let record = zup_transaction::FilesystemTransactionStore::private_at(&state_root)
        .load(&id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    let target = embedded
        .as_ref()
        .map(|build| build.installer.target)
        .unwrap_or_else(|| record.target);
    if target != record.target {
        return Err(miette::miette!(
            "this package targets `{target}` but the transaction targets `{}`",
            record.target
        ));
    }
    let backend = zup_windows::WindowsRuntimeBackend::for_recovery();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Recovery {
            transaction: id,
            request: recovery_request,
        },
        target,
        app_id: record.app_id.clone(),
        app_version: record.app_version,
        scope: record.scope,
        state_root,
    };
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
        action: LifecycleAction::Repair { force: false },
    };
    if OutputFormat::from(args.output) == OutputFormat::Human {
        execute::execute(prepared)
    } else {
        execute::execute_frontend(
            prepared,
            OutputFormat::from(args.output),
            LifecycleAction::Repair { force: false },
        )
        .map_err(Into::into)
    }
}

pub fn state_root(app: &AppId, scope: SelectedScope) -> miette::Result<PathBuf> {
    zup_windows::default_state_root(app, scope).map_err(|error| miette::miette!("{error}"))
}

pub fn resolve_state_root(
    app: &AppId,
    explicit: Option<PathBuf>,
    scope: SelectedScope,
) -> miette::Result<PathBuf> {
    zup_windows::resolve_state_root(app, explicit, scope)
        .map_err(|error| miette::miette!("{error}"))
}

pub fn peer_user_state_root(app: &AppId) -> miette::Result<PathBuf> {
    state_root(app, SelectedScope::User)
}

pub fn action_name(action: LifecycleAction) -> &'static str {
    match action {
        LifecycleAction::Cleanup { .. } => "cleanup",
        LifecycleAction::Install => "install",
        LifecycleAction::Upgrade => "upgrade",
        LifecycleAction::Downgrade => "downgrade",
        LifecycleAction::Rollback { .. } => "rollback",
        LifecycleAction::Modify => "modify",
        LifecycleAction::Repair { .. } => "repair",
        LifecycleAction::Uninstall => "uninstall",
    }
}
