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
    Build(BuildCommand),
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

fn default_install_scope(scope: zup_core::InstallScope) -> SelectedScope {
    match scope {
        zup_core::InstallScope::Machine => SelectedScope::Machine,
        zup_core::InstallScope::User | zup_core::InstallScope::Either => SelectedScope::User,
    }
}

#[derive(Debug, Args)]
struct ManifestCommand {
    #[arg(long, default_value = "zup.toml")]
    manifest: PathBuf,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
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
    app_id: Option<String>,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RecoverCommand {
    #[arg(long)]
    transaction_id: uuid::Uuid,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    payload_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct BuildCommand {
    #[arg(long, default_value = "zup.toml")]
    manifest: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    runtime: Option<PathBuf>,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Build(args)) => run_build(args)?,
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
        None => {
            let executable =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            if let Ok(bundle) = zup_bundle::EmbeddedBundle::open(&executable) {
                run_embedded_transition(
                    LifecycleAction::Install,
                    default_install_scope(bundle.plan().installer.install.scope),
                    None,
                    vec![],
                    vec![],
                )?;
            }
        }
    }
    Ok(())
}

fn state_root(path: &Path) -> miette::Result<PathBuf> {
    std::fs::create_dir_all(path).map_err(|error| miette::miette!("state root: {error}"))?;
    path.canonicalize()
        .map_err(|error| miette::miette!("state root: {error}"))
}

fn default_state_root(scope: SelectedScope) -> miette::Result<PathBuf> {
    let (variable, folder) = match scope {
        SelectedScope::User => ("LOCALAPPDATA", "zup"),
        SelectedScope::Machine => ("PROGRAMDATA", "zup"),
    };
    let base = std::env::var_os(variable)
        .map(PathBuf::from)
        .ok_or_else(|| miette::miette!("{variable} is not available"))?;
    if scope == SelectedScope::Machine {
        let base = base
            .canonicalize()
            .map_err(|error| miette::miette!("{variable}: {error}"))?;
        Ok(base.join(folder))
    } else {
        state_root(&base.join(folder))
    }
}

fn choose_state_root(path: Option<PathBuf>, scope: SelectedScope) -> miette::Result<PathBuf> {
    match path {
        Some(path) if scope == SelectedScope::Machine => {
            if path.exists() {
                state_root(&path)
            } else if path.is_absolute() {
                Ok(path)
            } else {
                Ok(std::env::current_dir()
                    .map_err(|e| miette::miette!("working directory: {e}"))?
                    .join(path))
            }
        }
        Some(path) => state_root(&path),
        None => default_state_root(scope),
    }
}

fn run_build(args: BuildCommand) -> miette::Result<()> {
    let manifest_path = args
        .manifest
        .canonicalize()
        .map_err(|e| miette::miette!("manifest: {e}"))?;
    let source =
        std::fs::read_to_string(&manifest_path).map_err(|e| miette::miette!("manifest: {e}"))?;
    let manifest = zup_manifest::parse(&source).map_err(|e| miette::miette!("manifest: {e}"))?;
    let installer =
        zup_manifest::parse_and_compile(&source).map_err(|e| miette::miette!("installer: {e}"))?;
    let build = zup_build::materialize(&manifest_path, &manifest, installer)
        .map_err(|e| miette::miette!("materialize: {e}"))?;
    let runtime = match args.runtime {
        Some(path) => path
            .canonicalize()
            .map_err(|e| miette::miette!("runtime: {e}"))?,
        None => zup_windows::current_exe().map_err(|e| miette::miette!("runtime: {e}"))?,
    };
    let output = args.output.unwrap_or_else(|| {
        let name: String = build
            .installer
            .app
            .name
            .as_str()
            .chars()
            .map(|character| {
                if character.is_control() || "<>:\"/\\|?*".contains(character) {
                    '-'
                } else {
                    character
                }
            })
            .collect();
        let name = name.trim_matches([' ', '.']);
        let name = if name.is_empty() {
            build.installer.app.id.as_str()
        } else {
            name
        };
        manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{name}-Setup.exe"))
    });
    let runtime_size = std::fs::metadata(&runtime)
        .map_err(|e| miette::miette!("runtime: {e}"))?
        .len();
    let (size, bundle_size) =
        zup_bundle::build_self_contained_executable(&runtime, &output, &build)
            .map_err(|e| miette::miette!("installer output: {e}"))?;
    println!(
        "{} ({} bytes total; {} bytes bundle and locator; {} bytes over runtime)",
        output.display(),
        size,
        bundle_size,
        size - runtime_size,
    );
    Ok(())
}

fn run_embedded_transition(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_bundle::EmbeddedBundle::open(&executable)
        .map_err(|e| miette::miette!("installer package: {e}"))?;
    let build = bundle
        .build_plan()
        .map_err(|e| miette::miette!("installer plan: {e}"))?;
    let app_id = build.installer.app.id.clone();
    let state_root = choose_state_root(state, scope)?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|e| miette::miette!("ledger: {e}"))?;
    if action == LifecycleAction::Uninstall {
        let ledger = prior.ok_or_else(|| miette::miette!("installation not found"))?;
        let execution =
            zup_windows::plan_target_lifecycle(action, &app_id, scope, None, &state_root)
                .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
        return execute(RuntimeRequest {
            app_id,
            app_version: ledger.version,
            scope,
            execution_plan: execution,
            work_root: state_root.join("work"),
            state_root,
            payload_root: executable,
            recovery_id: None,
        });
    }
    let mut request = zup_plan::PlanRequest::new(scope);
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
    for raw in enable {
        let id = ComponentId::new(&raw).map_err(|e| miette::miette!("component {raw}: {e}"))?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in disable {
        let id = ComponentId::new(&raw).map_err(|e| miette::miette!("component {raw}: {e}"))?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }
    let install = zup_plan::plan(&build, &request).map_err(|e| miette::miette!("plan: {e}"))?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|e| miette::miette!("target: {e}"))?;
    let execution =
        zup_windows::plan_target_lifecycle(action, &app_id, scope, Some(&target), &state_root)
            .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
    if action != LifecycleAction::Uninstall && scope == SelectedScope::User {
        publish_maintenance_candidate(
            &executable,
            &state_root,
            &app_id,
            scope,
            &target.app.version,
        )?;
    }
    let work_root = state_root.join("work");
    execute(RuntimeRequest {
        app_id,
        app_version: target.app.version.clone(),
        scope,
        execution_plan: execution,
        state_root,
        work_root,
        payload_root: executable,
        recovery_id: None,
    })
}

fn publish_maintenance_candidate(
    executable: &Path,
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &semver::Version,
) -> miette::Result<PathBuf> {
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let target = state_root
        .join("maintenance")
        .join(app_id.as_str())
        .join(scope_name)
        .join(version.to_string())
        .join("Setup.exe");
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| miette::miette!("maintenance directory: {e}"))?;
    }
    if target.exists() {
        let existing = zup_core::hash_reader(
            std::fs::File::open(&target)
                .map_err(|e| miette::miette!("maintenance package: {e}"))?,
        )
        .map_err(|e| miette::miette!("maintenance package: {e}"))?;
        let source = zup_core::hash_reader(
            std::fs::File::open(executable)
                .map_err(|e| miette::miette!("installer executable: {e}"))?,
        )
        .map_err(|e| miette::miette!("installer executable: {e}"))?;
        if existing != source {
            return Err(miette::miette!(
                "a different maintenance package already exists for version {version}"
            ));
        }
        return Ok(target);
    }
    zup_windows::copy_new_durable(executable, &target)
        .map_err(|e| miette::miette!("maintenance package publish: {e}"))?;
    Ok(target)
}

fn run_manifest_transition(action: LifecycleAction, args: ManifestCommand) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    if let Ok(bundle) = zup_bundle::EmbeddedBundle::open(&executable) {
        let scope = if bundle.plan().installer.install.scope == zup_core::InstallScope::Machine {
            SelectedScope::Machine
        } else {
            SelectedScope::from(args.scope)
        };
        return run_embedded_transition(action, scope, args.state_root, args.enable, args.disable);
    }
    let scope = SelectedScope::from(args.scope);
    let state_root = choose_state_root(args.state_root.clone(), scope)?;
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
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let embedded = zup_bundle::EmbeddedBundle::open(&executable).ok();
    let scope = embedded.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |bundle| {
            if bundle.plan().installer.install.scope == zup_core::InstallScope::Machine {
                SelectedScope::Machine
            } else {
                SelectedScope::from(args.scope)
            }
        },
    );
    let app_id = match args.app_id {
        Some(id) => AppId::new(&id).map_err(|error| miette::miette!("app ID: {error}"))?,
        None => embedded
            .as_ref()
            .ok_or_else(|| miette::miette!("installer package unavailable"))?
            .plan()
            .installer
            .app
            .id
            .clone(),
    };
    let state_root = choose_state_root(args.state_root, scope)?;
    if embedded.is_some() {
        return run_embedded_transition(
            LifecycleAction::Uninstall,
            scope,
            Some(state_root),
            vec![],
            vec![],
        );
    }
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
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_bundle::EmbeddedBundle::open(&executable).ok();
    let scope = bundle.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |bundle| default_install_scope(bundle.plan().installer.install.scope),
    );
    let state_root = choose_state_root(args.state_root, scope)?;
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
        && bundle.is_none()
    {
        return Err(miette::miette!(
            "--payload-root is required to recover a file installation"
        ));
    }
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let payload_root = args.payload_root.unwrap_or_else(|| {
        if bundle.is_some() {
            executable
        } else {
            state_root.join("unused-payload")
        }
    });
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
