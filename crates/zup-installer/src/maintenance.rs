use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueHint};
use zup_acquire::CachePolicy;
use zup_core::{AppId, Frontend, InstallScope, SelectedScope, Template};
use zup_exec::{InstallLedger, LifecycleAction};
use zup_presentation::{InstallerEvent, InstallerResult, OutputFormat, ProcessOutcome};
use zup_runtime::{ExecutionPolicy, RuntimeRequest};
use zup_transaction::{NodeKind, TransactionId, TransactionStore};
use zup_update::{ComponentSelection, TrustContext};

use crate::cli::{OutputArg, ScopeArg};
use crate::lifecycle::{self, PreparedRuntime, Request};
use crate::package;
use crate::run::RuntimeContext;
use crate::{acquire, execute};

#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub yes: bool,

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

    if args.ui {
        return Err(miette::miette!(
            "this build has no graphical surface; omit --ui or add --yes"
        ));
    }

    if context.is_live_console()
        && !args.non_interactive
        && !args.yes
        && args.output == OutputArg::Human
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
        args.scope.unwrap_or(ScopeArg::Either),
        args.state_root.as_deref(),
    )?;
    let output = args.output();
    let policy = if args.non_interactive
        || args.yes
        || args.output != OutputArg::Human
        || !context.is_live_console()
    {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
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
        remove_lock(&state_root, &app_id, scope)?;
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
        Request::Named(LifecycleAction::Uninstall),
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
        target: ledger.target.clone(),
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
    requested: ScopeArg,
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    let declared = installer.map(|installer| installer.install.scope);
    let allowed: &[SelectedScope] = match declared {
        Some(InstallScope::User) => &[SelectedScope::User],
        Some(InstallScope::Machine) => &[SelectedScope::Machine],
        Some(InstallScope::Either) | None => &[SelectedScope::User, SelectedScope::Machine],
    };
    if declared == Some(InstallScope::Machine) {
        return Ok((
            SelectedScope::Machine,
            resolve_state_root(state_root.map(Path::to_path_buf), SelectedScope::Machine)?,
        ));
    }
    let named = match requested {
        ScopeArg::User => Some(SelectedScope::User),
        ScopeArg::Machine => Some(SelectedScope::Machine),
        ScopeArg::Either => None,
    };
    if let Some(scope) = named {
        if !allowed.contains(&scope) {
            return Err(miette::miette!(
                "this application does not install into the {scope} scope"
            ));
        }
        return Ok((
            scope,
            resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    discover_scope(app_id, allowed, state_root)
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
    let scope = SelectedScope::User;
    Ok((
        scope,
        resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
    ))
}

pub fn remove_lock(state_root: &Path, app_id: &AppId, scope: SelectedScope) -> miette::Result<()> {
    let scope = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let key = zup_transaction::InstallationLock::lock_key(app_id.as_str(), scope);
    zup_transaction::InstallationLock::remove_if_unheld(state_root, &key)
        .map_err(|error| miette::miette!("remove the uninstall lock: {error}"))
}

/// in both cases waiting would mean the runner could never begin.
/// ending that process's tree must not end the runner's. Every other child Zup
pub fn launch_runner(
    executable: &Path,
    args: &UninstallArgs,
    ui: bool,
) -> miette::Result<std::process::Child> {
    let temporary =
        std::env::temp_dir().join(format!("zup-uninstall-{}.exe", uuid::Uuid::now_v7()));
    zup_windows::copy_new_durable(executable, &temporary)
        .map_err(|error| miette::miette!("prepare the uninstall runner: {error}"))?;
    let mut command = std::process::Command::new(&temporary);
    command
        .arg("__uninstall_runner")
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
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
    if args.output != OutputArg::Human {
        command.arg("--output").arg(args.output.as_str());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    command
        .spawn()
        .map_err(|error| miette::miette!("start the uninstall runner: {error}"))
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
            target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid"),
            frontend: Frontend::Headless,
            updates: None,
            install: zup_core::Install {
                scope: InstallScope::Machine,
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
        let root = tempfile::TempDir::new().expect("temp dir");
        let (scope, _) = resolve_scope(&app, Some(&installer), ScopeArg::User, Some(root.path()))
            .expect("a machine-wide application resolves without asking");
        assert_eq!(scope, SelectedScope::Machine);
    }
}

#[cfg(feature = "gui")]
use zup_preset_protocol::UpdateState;

#[derive(Debug, Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: Option<UpdateCommand>,
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub yes: bool,

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
    let _ = context;
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

    let (scope, state_root, ledger) = committed_installation(installer, &args)?;
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
    let (sink, _receiver) = zup_acquire::ProgressSink::channel(256);
    let context =
        TrustContext::from_update_config(config, installer.app.id.clone(), update_root.clone());
    let mut resolver = zup_update::ReleaseResolver::new(
        context,
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

    confirm(&args, output)?;
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
    let policy = if args.non_interactive || output != OutputFormat::Human {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
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
    let acquired = tokio
        .block_on(acquire::acquire(
            context,
            ComponentSelection::All,
            None,
            &[],
            sink,
        ))
        .map_err(|error| error.to_string())?;
    let available = acquired.resolved.descriptor.version.clone();
    let same_release = ledger
        .release_identity()
        .is_some_and(|installed| installed.release == acquired.resolved.descriptor.release_digest);
    if same_release {
        drop(emitter);
        status(UpdateState::UpToDate { current });
        return Ok(());
    }
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
    status(UpdateState::Available { current, available });
    Ok(())
}

fn committed_installation(
    installer: &zup_core::Installer,
    args: &UpdateArgs,
) -> miette::Result<(SelectedScope, PathBuf, zup_exec::InstallLedger)> {
    let mut scope = if installer.install.scope == zup_core::InstallScope::Machine {
        SelectedScope::Machine
    } else {
        args.scope
            .map(SelectedScope::from)
            .unwrap_or(SelectedScope::User)
    };
    let mut state_root = resolve_state_root(args.state_root.clone(), scope)?;
    let store = zup_windows::InstallLedgerStore::new(&state_root);
    let mut ledger = store
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?;
    if ledger.is_none()
        && args.scope.is_none()
        && args.state_root.is_none()
        && scope == SelectedScope::User
    {
        scope = SelectedScope::Machine;
        state_root = resolve_state_root(None, scope)?;
        ledger = store
            .load(&installer.app.id, scope)
            .map_err(|error| miette::miette!("installation ledger: {error}"))?;
    }
    let ledger = ledger.ok_or_else(|| miette::miette!("this application is not installed"))?;
    Ok((scope, state_root, ledger))
}

fn confirm(args: &UpdateArgs, output: OutputFormat) -> miette::Result<()> {
    if args.yes || args.non_interactive || output != OutputFormat::Human {
        return Ok(());
    }
    if crate::run::console_is_a_terminal() && !crate::frontend::confirm_update()? {
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
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,

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
    let scope = embedded.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |build| default_install_scope(build.installer.install.scope),
    );
    let state_root = resolve_state_root(args.state_root, scope)?;
    let id = TransactionId::from_uuid(args.transaction_id);
    let record = zup_transaction::FilesystemTransactionStore::new(&state_root)
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
        .map(|build| build.installer.target.clone())
        .unwrap_or_else(|| record.target.clone());
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

pub fn install_directory_template(path: &Path) -> miette::Result<Template> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(miette::miette!(
            "install directory must not contain template variables: {value}"
        ));
    }
    Template::parse(&value).map_err(|error| miette::miette!("install directory: {error}"))
}

pub fn persisted_install_directory(ledger: Option<&InstallLedger>) -> Option<Template> {
    ledger
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| Template::parse(&path.to_string()).ok())
}

pub fn choose_install_directory(
    explicit: Option<&Path>,
    ledger: Option<&InstallLedger>,
    allowed: bool,
) -> miette::Result<Option<Template>> {
    if let Some(path) = explicit {
        if !allowed {
            return Err(miette::miette!(
                "this application does not allow choosing an install directory"
            ));
        }
        return install_directory_template(path).map(Some);
    }
    if !allowed {
        return Ok(None);
    }
    Ok(persisted_install_directory(ledger))
}

pub fn default_install_scope(scope: InstallScope) -> SelectedScope {
    match scope {
        InstallScope::Machine => SelectedScope::Machine,
        InstallScope::User | InstallScope::Either => SelectedScope::User,
    }
}

pub fn resolve_applied_action(
    installed_version: Option<&semver::Version>,
    package_version: &str,
) -> miette::Result<LifecycleAction> {
    let package_version = semver::Version::parse(package_version).map_err(|error| {
        miette::miette!("this package's version `{package_version}` is not a version: {error}")
    })?;
    let Some(installed_version) = installed_version else {
        return Ok(LifecycleAction::Install);
    };
    match package_version.cmp(installed_version) {
        std::cmp::Ordering::Greater => Ok(LifecycleAction::Upgrade),
        std::cmp::Ordering::Equal => Ok(LifecycleAction::Modify),
        std::cmp::Ordering::Less => Err(miette::miette!(
            "downgrade from {installed_version} to {package_version} is refused"
        )),
    }
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

#[cfg(test)]
mod maintenance_tests {
    use super::*;

    #[test]
    fn applying_a_package_resolves_the_lifecycle_from_the_machine() {
        assert_eq!(
            resolve_applied_action(None, "2.0.0").expect("fresh install"),
            LifecycleAction::Install
        );
        let older = semver::Version::parse("1.0.0").expect("valid");
        assert_eq!(
            resolve_applied_action(Some(&older), "2.0.0").expect("newer package"),
            LifecycleAction::Upgrade
        );
        let same = semver::Version::parse("2.0.0").expect("valid");
        assert_eq!(
            resolve_applied_action(Some(&same), "2.0.0").expect("same version"),
            LifecycleAction::Modify
        );
        let newer = semver::Version::parse("3.0.0").expect("valid");
        let refusal = resolve_applied_action(Some(&newer), "2.0.0")
            .expect_err("a downgrade is refused")
            .to_string();
        assert!(refusal.contains("downgrade"), "{refusal}");
        assert!(refusal.contains("3.0.0"), "the refusal names both versions");

        assert!(resolve_applied_action(None, "not-a-version").is_err());
    }

    #[test]
    fn an_install_directory_the_application_forbids_is_refused_not_ignored() {
        let explicit = Path::new("/tmp/elsewhere");
        assert!(choose_install_directory(Some(explicit), None, false).is_err());
        assert!(choose_install_directory(Some(explicit), None, true).is_ok());
        assert!(
            choose_install_directory(None, None, false)
                .expect("nothing to choose")
                .is_none()
        );
        let error = install_directory_template(Path::new("/opt/${location.user_data}"))
            .expect_err("a template is not a chosen path")
            .to_string();
        assert!(error.contains("template variables"), "{error}");
    }
}
