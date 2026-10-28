//! path policy, and executes. Zup never handles an administrator password.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zup_core::{Frontend, SelectedScope};

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
    crate::terminal::command()
}

pub fn run(frontend: Frontend) -> miette::Result<()> {
    if frontend == Frontend::Gui {
        #[cfg(feature = "gui")]
        return run_gui();
        #[cfg(not(feature = "gui"))]
        return Err(miette::miette!(
            "this setup runtime has no graphical frontend"
        ));
    }
    // package: it authenticates and serves one session, never a lifecycle.
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    if std::env::args().any(|argument| argument == "__recover") {
        let cli = Cli::parse();
        if let Some(Verb::Recover(args)) = &cli.verb {
            return run_recovery(&cli, args.clone());
        }
    }
    crate::terminal::run(zup_linux::LinuxSessionHost::open(PathBuf::new()), frontend)
}

#[cfg(feature = "gui")]
fn run_gui() -> miette::Result<()> {
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let graphical = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
        || session_type.eq_ignore_ascii_case("wayland")
        || session_type.eq_ignore_ascii_case("x11");
    if !graphical {
        return Err(miette::miette!(
            "the graphical installer needs a Wayland or X11 session, and none is available \
             (no WAYLAND_DISPLAY or DISPLAY). Run the console installer instead, or start a \
             graphical session first"
        ));
    }
    let cli = Cli::parse();
    match &cli.verb {
        Some(Verb::__PrivilegedWorker(args)) => {
            return run_worker(
                args.session.clone(),
                args.client_pid,
                args.socket_path.clone(),
            );
        }
        Some(Verb::Recover(_)) => {
            return Err(miette::miette!(
                "recovery runs without a window; use the console installer"
            ));
        }
        _ => {}
    }
    if cli.verb.is_some() || cli.install_dir.is_some() || cli.state_root.is_some() {
        return Err(miette::miette!(
            "the graphical installer takes no operation arguments; choose the operation in the window"
        ));
    }
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let scope = cli.scope.map(ScopeFlag::selected);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("session runtime: {error}"))?
        .block_on(zup_session::run(
            zup_linux::LinuxSessionHost::open(executable.clone()),
            &executable,
            zup_session::SessionLaunch {
                scope,
                state_root: None,
                interactive: true,
                ..Default::default()
            },
        ))
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
    let store = zup_transaction::FilesystemTransactionStore::private_at(&state_root);
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
    let control = zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("session runtime: {error}"))?
        .block_on(zup_runtime::execute(&backend, request, &control));
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
