//! path policy, and executes. Zup never handles an administrator password.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};
use zup_core::{Frontend, SelectedScope};
use zup_session::SessionHost;

#[derive(Debug, Parser)]
#[command(name = "setup", version)]
struct Cli {
    #[command(subcommand)]
    verb: Option<Verb>,

    #[arg(long, global = true)]
    scope: Option<ScopeFlag>,

    /// scope only inside the machine program tree: an override never widens
    #[arg(long, global = true, value_name = "PATH")]
    install_dir: Option<PathBuf>,

    #[arg(long, global = true)]
    state_root: Option<PathBuf>,
}

/// The scope flag: an explicit choice, never a silent default to machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ScopeFlag {
    User,
    Machine,
}

impl ScopeFlag {
    fn selected(self) -> SelectedScope {
        match self {
            Self::User => SelectedScope::User,
            Self::Machine => SelectedScope::Machine,
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
enum Verb {
    Install(SelectionArgs),
    Upgrade(SelectionArgs),
    Modify(SelectionArgs),
    Repair(RepairArgs),
    Uninstall,
    #[command(name = "__recover", hide = true)]
    Recover(RecoverArgs),
    #[command(name = "__privileged-worker", hide = true)]
    __PrivilegedWorker(WorkerArgs),
}

#[derive(Debug, Clone, Default, Args)]
struct SelectionArgs {
    #[arg(long)]
    enable: Vec<String>,
    #[arg(long)]
    disable: Vec<String>,
}

#[derive(Debug, Clone, Args)]
struct RepairArgs {
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Args)]
struct RecoverArgs {
    #[arg(long)]
    transaction: String,
}

#[derive(Debug, Clone, Args)]
struct WorkerArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    client_pid: u32,
    #[arg(long)]
    socket_path: PathBuf,
}

pub fn command() -> clap::Command {
    Cli::command()
}

pub fn run(frontend: Frontend) -> miette::Result<()> {
    if frontend == Frontend::Gui {
        return Err(miette::miette!(
            "the graphical installer is not supported on Linux; use the console or headless installer"
        ));
    }
    // package: it authenticates and serves one session, never a lifecycle.
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    let cli = Cli::parse();
    match &cli.verb {
        Some(Verb::__PrivilegedWorker(args)) => {
            let args = args.clone();
            return run_worker(args.session, args.client_pid, args.socket_path);
        }
        Some(Verb::Recover(args)) => {
            return run_recovery(&cli, args.clone());
        }
        _ => {}
    }
    let selection_of = |selection: &SelectionArgs| zup_runtime::OperationSelection {
        enable: selection.enable.clone(),
        disable: selection.disable.clone(),
        install_directory: None,
    };
    let (operation, selection) = match cli.verb {
        None => (zup_runtime::Operation::Apply, Default::default()),
        Some(Verb::Install(selection)) => {
            (zup_runtime::Operation::Install, selection_of(&selection))
        }
        Some(Verb::Upgrade(selection)) => {
            (zup_runtime::Operation::Upgrade, selection_of(&selection))
        }
        Some(Verb::Modify(selection)) => (zup_runtime::Operation::Modify, selection_of(&selection)),
        Some(Verb::Repair(args)) => (
            zup_runtime::Operation::Repair {
                force_files: args.force,
            },
            Default::default(),
        ),
        Some(Verb::Uninstall) => (zup_runtime::Operation::Uninstall, Default::default()),
        Some(Verb::Recover(_)) | Some(Verb::__PrivilegedWorker(_)) => unreachable!("handled above"),
    };
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let mut selection = selection;
    if selection.install_directory.is_none() {
        selection.install_directory = cli.install_dir.clone();
    }
    let mut host = zup_linux::LinuxSessionHost::open(executable.clone());
    let discovery = host
        .discover(
            &executable,
            &zup_session::SessionLaunch {
                scope: cli.scope.map(ScopeFlag::selected),
                state_root: cli.state_root.clone(),
                ..Default::default()
            },
        )
        .map_err(|error| miette::miette!("{error}"))?;
    let state_root = discovery.state_root.clone();
    let input = zup_runtime::OperationInput {
        build: &discovery.build,
        installer: &executable,
        scope: discovery.scope,
        operation,
        selection,
        prior: discovery.prior(),
        state_root: &discovery.state_root,
        cancellation: &zup_plan::NeverCancelled,
    };
    let prepared = zup_runtime::prepare_operation(&input, &host)
        .map_err(|error| miette::miette!("{error}"))?;
    let (events, receiver) = tokio::sync::broadcast::channel(256);
    let pump = std::thread::spawn(move || {
        let mut receiver = receiver;
        while let Ok(event) = receiver.blocking_recv() {
            if zup_session::is_terminal(&event) {
                break;
            }
        }
    });
    let control = zup_runtime::RuntimeControl::new(
        zup_runtime::CancellationHandle::new(),
        events,
        zup_runtime::ExecutionPolicy::Interactive,
    );
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("session runtime: {error}"))?
        .block_on(zup_session::run_operation(
            &host, &discovery, &prepared, control,
        ));
    let _ = pump.join();
    match outcome {
        Ok(zup_runtime::InstallOutcome::Committed) => {
            println!("committed");
            Ok(())
        }
        Ok(zup_runtime::InstallOutcome::RolledBack) => {
            Err(miette::miette!("transaction: rolled back"))
        }
        Ok(zup_runtime::InstallOutcome::RecoveryRequired) => {
            let transaction = zup_runtime::discover_recovery(&state_root)
                .into_iter()
                .find_map(|status| match status {
                    zup_runtime::RecoveryStatus::RecoveryRequired { transaction_id } => {
                        Some(transaction_id)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "unknown".to_owned());
            Err(miette::miette!(
                "transaction {transaction} requires recovery"
            ))
        }
        Ok(zup_runtime::InstallOutcome::Busy { .. }) => {
            Err(miette::miette!("another maintenance operation is running"))
        }
        Ok(zup_runtime::InstallOutcome::Cancelled) => Err(miette::miette!("cancelled")),
        Ok(zup_runtime::InstallOutcome::RebootRequired { .. }) => {
            Err(miette::miette!("a reboot is required"))
        }
        Ok(zup_runtime::InstallOutcome::Failed(message)) => Err(miette::miette!("{message}")),
        Err(error) => Err(miette::miette!("{error}")),
    }
}

fn run_recovery(cli: &Cli, args: RecoverArgs) -> miette::Result<()> {
    let id = args
        .transaction
        .parse::<uuid::Uuid>()
        .map_err(|_| miette::miette!("the recovery transaction is named by uuid"))?;
    let id = zup_transaction::TransactionId::from_uuid(id);
    let scope = cli
        .scope
        .map(ScopeFlag::selected)
        .unwrap_or(SelectedScope::User);
    let state_root = match &cli.state_root {
        Some(root) => root.clone(),
        None => zup_linux::state_root(scope).map_err(|error| miette::miette!("{error}"))?,
    };
    let store = zup_transaction::FilesystemTransactionStore::new(&state_root);
    let record = zup_transaction::TransactionStore::load(&store, &id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    let executable = std::env::current_exe().map_err(|error| miette::miette!("{error}"))?;
    let backend = zup_linux::LinuxRuntimeBackend {
        installer: executable,
        operation: zup_runtime::Operation::Apply,
        selection: Default::default(),
        install_dir_override: None,
        generated: Default::default(),
    };
    let request = zup_runtime::RuntimeRequest {
        target: record.target,
        app_id: record.app_id.clone(),
        app_version: record.app_version.clone(),
        scope: record.scope,
        transaction_plan: record.plan.clone(),
        state_root: state_root.clone(),
        work_root: state_root.join("work"),
        recovery_id: Some(id),
        bootstrap: None,
        release: None,
    };
    let (events, _) = tokio::sync::broadcast::channel(16);
    let control = zup_runtime::RuntimeControl::new(
        zup_runtime::CancellationHandle::new(),
        events,
        zup_runtime::ExecutionPolicy::Interactive,
    );
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("session runtime: {error}"))?
        .block_on(async {
            use zup_runtime::RuntimeBackend;
            backend.execute(request, control).await
        });
    let _ = state_root;
    match outcome {
        Ok(zup_runtime::InstallOutcome::Committed) => {
            println!("committed");
            Ok(())
        }
        Ok(other) => Err(miette::miette!("recovery ended as {other:?}")),
        Err(error) => Err(miette::miette!("{error}")),
    }
}

fn worker_session_args() -> Option<(String, u32, PathBuf)> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "__privileged-worker" {
            let mut session = None;
            let mut client_pid = None;
            let mut socket_path = None;
            while let Some(flag) = args.next() {
                if flag == "--session" {
                    session = args.next();
                } else if flag == "--client-pid" {
                    client_pid = args.next().and_then(|pid| pid.parse::<u32>().ok());
                } else if flag == "--socket-path" {
                    socket_path = args.next().map(PathBuf::from);
                }
            }
            return session.map(|session| {
                (
                    session,
                    client_pid.unwrap_or(0),
                    socket_path.unwrap_or_default(),
                )
            });
        }
    }
    None
}

fn run_worker(session: String, client_pid: u32, socket_path: PathBuf) -> miette::Result<()> {
    let session: zup_protocol::SessionId = session
        .parse()
        .map_err(|_| miette::miette!("the worker serves one session, named by uuid"))?;
    match zup_linux::run_worker_mode(session, client_pid, &socket_path) {
        Ok(outcome) => {
            println!("worker: {outcome}");
            Ok(())
        }
        Err(error) => Err(miette::miette!("{error}")),
    }
}
