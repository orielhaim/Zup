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

#[derive(Debug, Clone, Subcommand)]
enum Verb {
    Install,
    Upgrade,
    Repair(RepairArgs),
    Uninstall,
    #[command(name = "__privileged-worker", hide = true)]
    __PrivilegedWorker(WorkerArgs),
}

#[derive(Debug, Clone, Args)]
struct RepairArgs {
    #[arg(long)]
    force: bool,
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

pub fn run(frontend: Frontend) -> miette::Result<()> {
    // console flow instead would report a window that never opened.
    if frontend == Frontend::Gui {
        return Err(miette::miette!(
            "the graphical installer is not supported on Linux in this phase; use the console or headless installer"
        ));
    }
    // package: it authenticates and serves one session, never a lifecycle.
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    let cli = Cli::parse();
    let action = match cli.verb {
        None => zup_linux::LinuxAction::Apply,
        Some(Verb::Install) => zup_linux::LinuxAction::Install,
        Some(Verb::Upgrade) => zup_linux::LinuxAction::Upgrade,
        Some(Verb::Repair(args)) => zup_linux::LinuxAction::Repair {
            force_files: args.force,
        },
        Some(Verb::Uninstall) => zup_linux::LinuxAction::Uninstall,
        Some(Verb::__PrivilegedWorker(args)) => {
            return run_worker(args.session, args.client_pid, args.socket_path);
        }
    };
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let scope = resolve_scope(&executable, cli.scope)?;
    match zup_linux::run(&zup_linux::LinuxRunRequest {
        installer: executable,
        scope,
        state_root: cli.state_root,
        action,
        install_dir_override: cli.install_dir,
    }) {
        Ok(zup_linux::LinuxOutcome::Committed { .. }) => {
            println!("committed");
            Ok(())
        }
        Ok(zup_linux::LinuxOutcome::RolledBack) => Err(miette::miette!("transaction: rolled back")),
        Ok(zup_linux::LinuxOutcome::RecoveryRequired { transaction }) => Err(miette::miette!(
            "transaction {transaction} requires recovery"
        )),
        Ok(zup_linux::LinuxOutcome::Busy) => {
            Err(miette::miette!("another maintenance operation is running"))
        }
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

/// never the silent answer.
fn resolve_scope(executable: &PathBuf, flag: Option<ScopeFlag>) -> miette::Result<SelectedScope> {
    let carrier = zup_linux::Carrier::open(executable)
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    let mut targets = carrier
        .package()
        .build_plan()
        .map_err(|error| miette::miette!("installer package: {error}"))?
        .targets;
    if targets.len() != 1 {
        return Err(miette::miette!(
            "an installer package holds exactly one target; this one holds {}",
            targets.len()
        ));
    }
    let declared = targets.remove(0).installer.install.scope;
    let requested = match flag {
        Some(ScopeFlag::User) => Some(SelectedScope::User),
        Some(ScopeFlag::Machine) => Some(SelectedScope::Machine),
        None => None,
    };
    match (declared, requested) {
        (zup_core::InstallScope::User, None) => Ok(SelectedScope::User),
        (zup_core::InstallScope::Machine, None) => Ok(SelectedScope::Machine),
        (zup_core::InstallScope::Either, None) => Err(miette::miette!(
            "this installer serves the current user or the whole machine: choose with --scope user or --scope machine"
        )),
        (declared, Some(scope)) if allows(declared, scope) => Ok(scope),
        (declared, Some(_)) => Err(miette::miette!(
            "this installer does not serve the requested scope (manifest scope: {declared})"
        )),
    }
}

fn allows(declared: zup_core::InstallScope, scope: SelectedScope) -> bool {
    match scope {
        SelectedScope::User => declared.allows_user(),
        SelectedScope::Machine => declared.allows_machine(),
    }
}
