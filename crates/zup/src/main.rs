//! Developer-facing CLI for zup.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use zup_core::{AppId, ComponentId, SelectedScope};
use zup_exec::{LifecycleAction, RemovalKind};
use zup_runtime::{InstallOutcome, RuntimeRequest};

/// Process entry point for the internal worker mode.
#[derive(Debug, Parser)]
#[command(
    name = "zup",
    version,
    about = "A programmable application installer for the modern desktop",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Install(ManifestCommand),
    Upgrade(ManifestCommand),
    Modify(ManifestCommand),
    Repair(RepairCommand),
    Uninstall(UninstallCommand),
    Recover(RecoverCommand),
    #[command(name = "__worker", hide = true)]
    Worker {
        bootstrap: String,
    },
    /// Print the protocol/worker bootstrap format for tests.
    #[command(hide = true)]
    WorkerHelp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    User,
    Machine,
}

impl From<ScopeArg> for SelectedScope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::User => Self::User,
            ScopeArg::Machine => Self::Machine,
        }
    }
}

#[derive(Debug, Args)]
struct ManifestCommand {
    #[arg(long, default_value = "zup.toml")]
    manifest: PathBuf,
    #[arg(long, value_enum)]
    scope: ScopeArg,
    #[arg(long)]
    state_root: PathBuf,
    #[arg(long)]
    work_root: Option<PathBuf>,
    #[arg(long = "enable")]
    enable: Vec<String>,
    #[arg(long = "disable")]
    disable: Vec<String>,
}

#[derive(Debug, Args)]
struct RepairCommand {
    #[command(flatten)]
    install: ManifestCommand,
    #[arg(long)]
    force_files: bool,
}

#[derive(Debug, Args)]
struct UninstallCommand {
    #[arg(long)]
    app_id: String,
    #[arg(long, value_enum)]
    scope: ScopeArg,
    #[arg(long)]
    state_root: PathBuf,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RecoverCommand {
    #[arg(long)]
    transaction_id: uuid::Uuid,
    #[arg(long)]
    state_root: PathBuf,
    #[arg(long)]
    payload_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Install(args)) => run_manifest_transition(LifecycleAction::Install, args)?,
        Some(Commands::Upgrade(args)) => run_manifest_transition(LifecycleAction::Upgrade, args)?,
        Some(Commands::Modify(args)) => run_manifest_transition(LifecycleAction::Modify, args)?,
        Some(Commands::Repair(args)) => run_manifest_transition(
            LifecycleAction::Repair {
                force_files: args.force_files,
            },
            args.install,
        )?,
        Some(Commands::Uninstall(args)) => run_uninstall(args)?,
        Some(Commands::Recover(args)) => run_recover(args)?,
        Some(Commands::Worker { bootstrap }) => run_worker_mode(&bootstrap)?,
        Some(Commands::WorkerHelp) => {
            println!(
                "zup __worker <protocol>|<session>|<pipe>|<parent_pid>|<parent_sid>|<plan_hash>"
            );
        }
        None => {}
    }
    Ok(())
}

fn state_root(path: &Path) -> miette::Result<PathBuf> {
    std::fs::create_dir_all(path).map_err(|error| miette::miette!("state root: {error}"))?;
    path.canonicalize()
        .map_err(|error| miette::miette!("state root: {error}"))
}

fn run_manifest_transition(action: LifecycleAction, args: ManifestCommand) -> miette::Result<()> {
    let scope = SelectedScope::from(args.scope);
    let state_root = state_root(&args.state_root)?;
    let manifest_path = args
        .manifest
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source = std::fs::read_to_string(&manifest_path)
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let manifest =
        zup_manifest::parse(&source).map_err(|error| miette::miette!("manifest: {error}"))?;
    let installer = zup_manifest::parse_and_compile(&source)
        .map_err(|error| miette::miette!("installer: {error}"))?;
    let app_id = installer.app.id.clone();
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let build = zup_build::materialize(&manifest_path, &manifest, installer)
        .map_err(|error| miette::miette!("materialize: {error}"))?;
    let mut request = zup_plan::PlanRequest::new(scope);
    if matches!(action, LifecycleAction::Repair { .. })
        && (!args.enable.is_empty() || !args.disable.is_empty())
    {
        return Err(miette::miette!(
            "repair uses the committed component selection"
        ));
    }
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous = prior
            .as_ref()
            .ok_or_else(|| miette::miette!("installation not found"))?;
        for component in &build.installer.components {
            if previous.selected_components.contains(&component.id) {
                request.components.enable.insert(component.id.clone());
            } else if !component.required {
                request.components.disable.insert(component.id.clone());
            }
        }
    }
    for raw in args.enable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in args.disable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }
    let install =
        zup_plan::plan(&build, &request).map_err(|error| miette::miette!("plan: {error}"))?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let execution =
        zup_windows::plan_target_lifecycle(action, &app_id, scope, Some(&target), &state_root)
            .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
    let payload_root = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&manifest.source.directory);
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    execute(RuntimeRequest {
        app_id,
        app_version: target.app.version.clone(),
        scope,
        execution_plan: execution,
        state_root,
        work_root,
        payload_root,
        recovery_id: None,
    })
}

fn run_uninstall(args: UninstallCommand) -> miette::Result<()> {
    let scope = SelectedScope::from(args.scope);
    let app_id = AppId::new(&args.app_id).map_err(|error| miette::miette!("app ID: {error}"))?;
    let state_root = state_root(&args.state_root)?;
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    let execution = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        scope,
        None,
        &state_root,
    )
    .map_err(|error| miette::miette!("uninstall plan: {error}"))?;
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    execute(RuntimeRequest {
        app_id,
        app_version: ledger.version,
        scope,
        execution_plan: execution,
        payload_root: state_root.join("unused-payload"),
        state_root,
        work_root,
        recovery_id: None,
    })
}

fn run_recover(args: RecoverCommand) -> miette::Result<()> {
    use zup_transaction::TransactionStore;
    let state_root = state_root(&args.state_root)?;
    let id = zup_transaction::TransactionId::from_uuid(args.transaction_id);
    let record = zup_transaction::FilesystemTransactionStore::new(&state_root)
        .load(&id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    if record
        .plan
        .nodes
        .iter()
        .any(|node| matches!(node.kind, zup_transaction::NodeKind::StageFile { .. }))
        && args.payload_root.is_none()
    {
        return Err(miette::miette!(
            "--payload-root is required to recover a file installation"
        ));
    }
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let payload_root = args
        .payload_root
        .unwrap_or_else(|| state_root.join("unused-payload"));
    execute(RuntimeRequest {
        app_id: record.app_id,
        app_version: record.app_version,
        scope: record.scope,
        execution_plan: zup_exec::ExecutionPlan::default(),
        state_root,
        work_root,
        payload_root,
        recovery_id: Some(id),
    })
}

fn execute(request: RuntimeRequest) -> miette::Result<()> {
    let drifted: Vec<String> = request
        .execution_plan
        .removals
        .iter()
        .filter(|op| op.kind == RemovalKind::Drift)
        .map(|op| format!("{:?}", op.key))
        .collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("runtime: {error}"))?;
    let (outcome, _) = runtime
        .block_on(zup_runtime::run_install(request))
        .map_err(|error| miette::miette!("install session: {error}"))?;
    match outcome {
        InstallOutcome::Committed => {
            println!("committed");
            for key in drifted {
                eprintln!("left drifted resource untouched: {key}");
            }
            Ok(())
        }
        other => Err(miette::miette!("transaction: {other:?}")),
    }
}

/// Hidden elevated/unelevated worker entry.
fn run_worker_mode(bootstrap_arg: &str) -> miette::Result<()> {
    let bootstrap = zup_windows::parse_bootstrap(bootstrap_arg)
        .map_err(|e| miette::miette!("worker bootstrap rejected: {e}"))?;

    if bootstrap.expected_parent_pid == 0 {
        return Err(miette::miette!(
            "worker bootstrap rejected: zero parent pid"
        ));
    }

    // Run the real worker runtime (connect → authenticate → execute → exit).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("tokio runtime: {e}"))?;

    let cancel = tokio_util::sync::CancellationToken::new();
    rt.block_on(zup_windows::run_worker(bootstrap, cancel))
        .map_err(|e| miette::miette!("worker failed: {e}"))
        .map(|_outcome| ())
}
