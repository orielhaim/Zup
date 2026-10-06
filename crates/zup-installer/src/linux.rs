//! The Linux command surface: one installer binary, five verbs.
//!
//! Deliberately small. The Windows CLI carries acquisition, handoff,
//! prerequisites, presets, and a worker protocol; the Linux Phase 2 installer
//! carries a package and runs it in this process. Every verb maps onto one
//! [`zup_linux::LinuxAction`], and the exit codes reuse the stable outcome
//! vocabulary so a script branches on them the same way.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zup_core::Frontend;

/// The installer command line on Linux.
///
/// No arguments means apply: the machine's own record resolves the verb, so a
/// double-clicked installer and a re-run installer do the same thing without
/// being told which.
#[derive(Debug, Parser)]
#[command(name = "setup", version)]
struct Cli {
    #[command(subcommand)]
    verb: Option<Verb>,

    /// Use this state root instead of the scope's own.
    ///
    /// The only correct answer outside a test is the scope's root, which is
    /// what an absent flag resolves to. The flag exists so an isolated
    /// environment can prove the installer without touching a real profile.
    #[arg(long, global = true)]
    state_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Subcommand)]
enum Verb {
    /// Install this package.
    Install,
    /// Upgrade an existing installation to this package.
    Upgrade,
    /// Restore owned files from the maintenance copy.
    Repair(RepairArgs),
    /// Remove the installation.
    Uninstall,
}

#[derive(Debug, Clone, Args)]
struct RepairArgs {
    /// Also restore files that are present but different.
    ///
    /// A present-but-different file may be damage or a user edit, and without
    /// this flag the installer refuses to decide which silently.
    #[arg(long)]
    force: bool,
}

/// Run the runtime as the frontend its binary was built for.
pub fn run(frontend: Frontend) -> miette::Result<()> {
    // GUI has no Linux presenter in this phase, and silently running the
    // console flow instead would report a window that never opened.
    if frontend == Frontend::Gui {
        return Err(miette::miette!(
            "the graphical installer is not supported on Linux in this phase; use the console or headless installer"
        ));
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
    };
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    match zup_linux::run(&zup_linux::LinuxRunRequest {
        installer: executable,
        scope: zup_core::SelectedScope::User,
        state_root: cli.state_root,
        action,
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
