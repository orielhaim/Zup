use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueHint};
use zup_acquire::CachePolicy;
use zup_core::{AppId, Frontend, SelectedScope};
use zup_exec::LifecycleAction;
use zup_presentation::{InstallerEvent, InstallerResult, OutputFormat, ProcessOutcome};
use zup_runtime::{ExecutionPolicy, RuntimeRequest};
use zup_transaction::{NodeKind, TransactionId, TransactionStore};
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
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
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

    if zup_transaction::is_maintenance_path(&executable) {
        launch_runner(&executable, &args, false)?;
        return Ok(());
    }

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
        return Err(miette::miette!(
            "this build has no graphical surface; omit --ui or add --yes"
        ));
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
            args.work_root.clone(),
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
    work_root: Option<PathBuf>,
    frontend: Option<Frontend>,
) -> miette::Result<PreparedRuntime> {
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
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
    let work_root = work_root.unwrap_or_else(|| state_root.join("work"));
    // never reads a payload file, because every file it removes is named by the
    let payload_root =
        std::env::current_dir().map_err(|error| miette::miette!("working directory: {error}"))?;
    let request = RuntimeRequest {
        target: ledger.target,
        app_id: app_id.clone(),
        app_version: ledger.version,
        scope,
        transaction_plan: execution,
        state_root: state_root.clone(),
        work_root,
        recovery_id: None,
        release: None,
        bootstrap: None,
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
            resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    if let Some(scope) = requested {
        return Ok((
            scope,
            resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
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
        let root = resolve_state_root(state_root.map(Path::to_path_buf), scope)?;
        let installed = zup_windows::InstallLedgerStore::new(&root)
            .load(app_id, scope)
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

/// in both cases waiting would mean the runner could never begin.
/// ending that process's tree must not end the runner's. Every other child Zup
pub fn launch_runner(
    executable: &Path,
    args: &UninstallArgs,
    ui: bool,
) -> miette::Result<std::process::Child> {
    let interactive = crate::run::console_is_a_terminal()
        && !args.non_interactive
        && !args.yes
        && args.output == Output::Human;
    let mut command = runner_command(executable, "__uninstall_runner", interactive)?;
    if ui {
        command.arg("--ui");
    }
    if let Some(scope) = args.scope {
        command.arg("--scope").arg(scope.as_str());
    }
    if let Some(value) = &args.app_id {
        command.arg("--app-id").arg(value);
    }
    if let Some(value) = &args.state_root {
        command.arg("--state-root").arg(value);
    }
    if let Some(value) = &args.work_root {
        command.arg("--work-root").arg(value);
    }
    if args.non_interactive {
        command.arg("--non-interactive");
    }
    if args.yes {
        command.arg("--yes");
    }
    if args.plain {
        command.arg("--plain");
    }
    if args.output != Output::Human {
        command.arg("--output").arg(args.output.as_str());
    }
    command
        .spawn()
        .map_err(|error| miette::miette!("start the uninstall runner: {error}"))
}

fn runner_command(
    executable: &Path,
    kind: &str,
    interactive: bool,
) -> miette::Result<std::process::Command> {
    use std::os::windows::process::CommandExt;
    let temporary =
        std::env::temp_dir().join(format!("zup-maintenance-{}.exe", uuid::Uuid::now_v7()));
    zup_windows::copy_new_durable(executable, &temporary)
        .map_err(|error| miette::miette!("prepare the maintenance helper: {error}"))?;
    let mut command = std::process::Command::new(temporary);
    command
        .arg(kind)
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    command.creation_flags(if interactive {
        CREATE_NEW_CONSOLE
    } else {
        DETACHED_PROCESS
    });
    Ok(command)
}

pub fn launch_terminal_runner(executable: &Path) -> miette::Result<()> {
    runner_command(executable, "__terminal_runner", true)?
        .arg("--")
        .args(std::env::args_os().skip(1))
        .spawn()
        .map(|_| ())
        .map_err(|error| miette::miette!("start the maintenance terminal: {error}"))
}

pub fn schedule_runner_cleanup(path: &Path) {
    let path = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue",
        std::process::id(),
        path
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn();
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
            root.ends_with("zup"),
            "machine scope uses the authoritative state root"
        );
        assert_eq!(scope, SelectedScope::Machine);
        assert!(resolve_scope(&app, Some(&installer), Some(SelectedScope::User), None).is_err());
    }
}

#[cfg(feature = "gui")]
use zup_preset_protocol::UpdateState;

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
        peer_user_state_root()?
    } else {
        state_root.clone()
    };

    if output == OutputFormat::Jsonl && !machine_run {
        println!(
            "{}",
            serde_json::to_string(&InstallerEvent::started(
                installer.app.id.as_str(),
                ledger.version.to_string(),
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
    let trust =
        TrustContext::from_update_config(config, installer.app.id.clone(), update_root.clone());
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

    let same_release = installed.as_ref().is_some_and(|installed| {
        installed.release == resolved.descriptor.release_digest
            && installed.catalog == resolved.descriptor.catalog.digest
    });
    let newer = match &installed {
        Some(installed) => acquire::is_newer(installed, &resolved.descriptor.version),
        None => true,
    };
    if same_release || !newer {
        return up_to_date(
            output,
            installer.app.id.as_str(),
            ledger.version.to_string(),
            scope,
            machine_run,
        );
    }
    if check_only {
        return update_available(
            output,
            installer.app.id.as_str(),
            ledger.version.to_string(),
            resolved.descriptor.version.clone(),
            scope,
        );
    }

    confirm(context, &args)?;
    if output == OutputFormat::Human {
        println!(
            "update available: {} → {}",
            ledger.version, resolved.descriptor.version
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
        .map_err(|error| {
            debug_assert!(error.left_machine_unchanged());
            miette::miette!("update acquisition: {error}")
        })?;
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
            scope: Some(scope),
            ..acquire::Request::new(LifecycleAction::Upgrade)
        },
        &acquired,
        output,
        policy,
    )
}

#[cfg(feature = "gui")]
pub fn from_maintenance_surface(
    executable: &Path,
    scope: SelectedScope,
    request: &zup_runtime::UpdateRequest,
    status: &mut dyn FnMut(UpdateState),
) -> Result<(), String> {
    let bundle = package::open_bundle(executable).map_err(|error| error.to_string())?;
    let build = package::target_plan(&bundle).map_err(|error| error.to_string())?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| "this application is not configured for updates".to_owned())?;
    let root = resolve_state_root(None, scope).map_err(|error| error.to_string())?;
    let ledger = zup_windows::InstallLedgerStore::new(&root)
        .load(&installer.app.id, scope)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the installed application was not found".to_owned())?;
    let current = ledger.version.to_string();
    let update_root = if scope == SelectedScope::Machine {
        peer_user_state_root().map_err(|error| error.to_string())?
    } else {
        root.clone()
    };
    let context = TrustContext::from_update_config(config, installer.app.id.clone(), update_root);
    let (sink, receiver) = zup_acquire::ProgressSink::channel(64);
    let emitter = zup_presentation::acquisition_thread(receiver);
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    status(UpdateState::Checking {
        detail: "Resolving the update channel…".into(),
    });
    let resolver = zup_update::ReleaseResolver::new(
        context,
        zup_acquire::HostProfile::native(),
        CachePolicy::Auto,
        sink,
    )
    .map_err(|error| error.to_string())?;
    let resolved = tokio
        .block_on(resolver.resolve())
        .map_err(|error| error.to_string())?;
    let available = resolved.descriptor.version.clone();
    if semver::Version::parse(&available).map_err(|error| error.to_string())? <= ledger.version {
        status(UpdateState::UpToDate { current });
        return Ok(());
    }
    if matches!(request, zup_runtime::UpdateRequest::Check) {
        status(UpdateState::Available { current, available });
        return Ok(());
    }
    if let zup_runtime::UpdateRequest::Install {
        version: Some(expected),
    } = request
        && expected != &available
    {
        return Err(
            "The available update changed. Check for updates again before installing.".into(),
        );
    }
    let acquired = tokio
        .block_on(acquire::acquire_closure(
            &resolver,
            resolved,
            ComponentSelection::All,
        ))
        .map_err(|error| error.to_string())?;
    status(UpdateState::Installing {
        detail: acquired.estimate().to_string(),
    });
    lifecycle::run_acquired_transition(
        acquire::Request {
            scope: Some(scope),
            ..acquire::Request::new(LifecycleAction::Upgrade)
        },
        &acquired,
        OutputFormat::Human,
        ExecutionPolicy::Interactive,
    )
    .map_err(|error| error.to_string())?;
    let _ = emitter;
    status(UpdateState::UpToDate { current: available });
    Ok(())
}

fn committed_installation(
    context: RuntimeContext,
    installer: &zup_core::Installer,
    args: &UpdateArgs,
) -> miette::Result<(SelectedScope, PathBuf, zup_exec::InstallLedger)> {
    let scope = crate::terminal::choose_scope(
        installer,
        args.scope.map(Into::into),
        context.may_prompt(args.non_interactive, args.yes),
        args.plain,
    )?;
    let state_root = resolve_state_root(args.state_root.clone(), scope)?;
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
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
    #[arg(long)]
    pub transaction_id: uuid::Uuid,
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: Output,

    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub payload_root: Option<PathBuf>,
    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
}

pub fn run_recovery(context: RuntimeContext, args: RecoveryArgs) -> miette::Result<()> {
    let _ = context;
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
    let state_root = resolve_state_root(args.state_root, scope)?;
    let id = TransactionId::from_uuid(args.transaction_id);
    if scope == SelectedScope::Machine {
        let build = embedded.ok_or_else(|| {
            miette::miette!("machine recovery uses the package embedded in this installer")
        })?;
        let plan = zup_transaction::compile_transaction(&zup_transaction::TransactionInput::new(
            build.installer.target,
        ))
        .map_err(|error| miette::miette!("transaction: {error}"))?;
        let request = RuntimeRequest {
            target: build.installer.target,
            app_id: build.installer.app.id.clone(),
            app_version: build.installer.app.version.clone(),
            scope,
            transaction_plan: plan,
            state_root,
            work_root: args.work_root.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("zup-work-{}", args.transaction_id))
            }),
            recovery_id: Some(id),
            release: None,
            bootstrap: None,
        };
        let backend = zup_windows::WindowsRuntimeBackend::for_recovery(
            args.payload_root.unwrap_or(executable),
            &request.state_root,
            scope,
            id,
        )
        .map_err(|error| miette::miette!("payload recovery: {error}"))?;
        let prepared = PreparedRuntime {
            request,
            backend: std::sync::Arc::new(backend),
            action: LifecycleAction::Repair { force_files: false },
        };
        return if OutputFormat::from(args.output) == OutputFormat::Human {
            execute::execute(prepared)
        } else {
            execute::execute_frontend(
                prepared,
                OutputFormat::from(args.output),
                LifecycleAction::Repair { force_files: false },
            )
            .map_err(Into::into)
        };
    }
    let record = zup_transaction::FilesystemTransactionStore::private_at(&state_root)
        .load(&id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    let stages_files = record
        .plan
        .nodes
        .iter()
        .any(|node| matches!(node.kind, NodeKind::StageFile { .. }));
    if stages_files && args.payload_root.is_none() && bundle.is_none() {
        return Err(miette::miette!(
            "a file transaction needs the payload it staged; pass --payload-root"
        ));
    }
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let payload_root = args.payload_root.unwrap_or_else(|| {
        if bundle.is_some() {
            executable
        } else {
            std::env::current_dir().unwrap_or_else(|_| state_root.clone())
        }
    });
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
    let backend = zup_windows::WindowsRuntimeBackend::for_recovery(
        payload_root,
        &state_root,
        record.scope,
        id,
    )
    .map_err(|error| miette::miette!("payload recovery: {error}"))?;
    let request = RuntimeRequest {
        target,
        app_id: record.app_id.clone(),
        app_version: record.app_version,
        scope: record.scope,
        transaction_plan: record.plan.clone(),
        state_root,
        work_root,
        recovery_id: Some(id),
        bootstrap: None,
        release: None,
    };
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
        action: LifecycleAction::Repair { force_files: false },
    };
    if OutputFormat::from(args.output) == OutputFormat::Human {
        execute::execute(prepared)
    } else {
        execute::execute_frontend(
            prepared,
            OutputFormat::from(args.output),
            LifecycleAction::Repair { force_files: false },
        )
        .map_err(Into::into)
    }
}

pub fn state_root(scope: SelectedScope) -> miette::Result<PathBuf> {
    zup_windows::default_state_root(scope).map_err(|error| miette::miette!("{error}"))
}

pub fn resolve_state_root(
    explicit: Option<PathBuf>,
    scope: SelectedScope,
) -> miette::Result<PathBuf> {
    zup_windows::resolve_state_root(explicit, scope).map_err(|error| miette::miette!("{error}"))
}

pub fn peer_user_state_root() -> miette::Result<PathBuf> {
    state_root(SelectedScope::User)
}

pub fn action_name(action: LifecycleAction) -> &'static str {
    match action {
        LifecycleAction::Install => "install",
        LifecycleAction::Upgrade => "upgrade",
        LifecycleAction::Modify => "modify",
        LifecycleAction::Repair { .. } => "repair",
        LifecycleAction::Uninstall => "uninstall",
    }
}
