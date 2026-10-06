//! The runtime's public command surface.
//!
//! This parser belongs to the application runtime and to nothing else. It knows
//! about installing, maintaining, and removing one application, and about the
//! process boundaries the runtime itself spawns. It has no build verbs, no
//! manifest verbs, and no publish verbs, because the executable carrying it is
//! shipped to an end user's machine and those verbs would be capabilities the
//! product does not have.
//!
//! Two kinds of command live here, and they are not the same thing:
//!
//! - **Public.** What a person or an automation system is expected to run, and
//!   what `--help` lists.
//! - **Internal.** Process boundaries the runtime spawns itself - the elevated
//!   worker, the uninstall runner, explicit recovery, the upgrade verb a
//!   framework updater contract needs. Hidden, because naming them in help is an
//!   invitation to depend on an implementation detail, and because showing them
//!   in the developer's CLI would be a category error.

use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};

use zup_presentation::OutputFormat;

use crate::context::RuntimeContext;
use crate::frontend;
use crate::lifecycle;

/// The setup runtime, addressed by whatever name the file has.
///
/// A generated installer is renamed before a user ever sees it, so the parser
/// takes its identity from `argv[0]` rather than from the Cargo binary name.
pub fn invoked_name() -> String {
    std::env::args_os()
        .next()
        .map(std::path::PathBuf::from)
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Setup".to_owned())
}

/// The invoked name, computed once and held for the process.
///
/// `clap` stores a command's name in a type that is either a `&'static str` or an
/// interned one, so a name read from `argv[0]` has to live as long as the parser.
fn command_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(invoked_name).as_str()
}

/// The runtime parser, named for the executable it was invoked as.
pub fn parser() -> clap::Command {
    Cli::command()
        .name(command_name())
        .about("Install and maintain this application.")
}

/// Parse `argv`, exiting the way a CLI exits on `--help` or a usage error.
pub fn parse() -> Cli {
    let matches = parser().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// The command names the runtime's public surface exposes, in help order.
#[cfg(test)]
pub fn public_commands() -> Vec<String> {
    parser()
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect()
}

/// The command names the runtime keeps for its own process boundaries.
#[cfg(test)]
pub fn internal_commands() -> Vec<String> {
    parser()
        .get_subcommands()
        .filter(|command| command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect()
}

/// Run the runtime from an already-parsed command line.
pub fn dispatch(cli: Cli, context: RuntimeContext) -> miette::Result<()> {
    match cli.command {
        Some(Commands::Install(args)) => lifecycle::apply(context, args),
        Some(Commands::Modify(args)) => lifecycle::run(context, lifecycle::Verb::Modify, args),
        Some(Commands::Repair(args)) => lifecycle::run(
            context,
            lifecycle::Verb::Repair {
                force_files: args.force_files,
            },
            args.install,
        ),
        Some(Commands::Update(args)) => crate::update::run(context, args),
        Some(Commands::Uninstall(args)) => crate::uninstall::run(context, args),
        Some(Commands::Worker { bootstrap }) => crate::worker::run(&bootstrap),
        Some(Commands::UninstallRunner(args)) => {
            zup_windows::wait_for_process_exit(args.wait_pid)
                .map_err(|error| miette::miette!("wait for maintenance process: {error}"))?;
            let cleanup =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            let result = if args.uninstall.ui {
                uninstall_confirmation(context, &args.uninstall)
            } else {
                crate::uninstall::run(context, args.uninstall)
            };
            crate::uninstall::schedule_runner_cleanup(&cleanup);
            result
        }
        Some(Commands::Upgrade(args)) => lifecycle::run(context, lifecycle::Verb::Upgrade, args),
        Some(Commands::Recover(args)) => crate::recovery::run(context, args),
        None => lifecycle::direct_launch(context),
    }
}

/// The uninstall confirmation window, for Apps & Features on a GUI frontend.
///
/// A separate process, because the file that would host it is the file Windows is
/// about to delete.
fn uninstall_confirmation(
    context: RuntimeContext,
    args: &crate::uninstall::UninstallArgs,
) -> miette::Result<()> {
    let executable =
        zup_windows::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(&executable)
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    frontend::graphical_uninstall(context, &executable, &bundle, args)
}

/// The setup runtime.
#[derive(Debug, Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Apply this package: install it, or bring an installed application up to it.
    Install(LifecycleArgs),
    /// Change which components an installed application has.
    Modify(LifecycleArgs),
    /// Restore the resources this application owns that have drifted.
    Repair(RepairArgs),
    /// Resolve a verified release and install what changed.
    Update(crate::update::UpdateArgs),
    /// Remove the application and the resources it owns.
    Uninstall(crate::uninstall::UninstallArgs),
    /// The elevated worker this runtime spawns for privileged work.
    #[command(name = "__worker", hide = true)]
    Worker { bootstrap: String },
    /// The out-of-process uninstall runner, which waits for this process first.
    #[command(name = "__uninstall_runner", hide = true)]
    UninstallRunner(UninstallRunnerArgs),
    /// The upgrade verb, for a framework updater contract that must name it.
    #[command(name = "__upgrade", hide = true)]
    Upgrade(LifecycleArgs),
    /// Reconcile one interrupted transaction.
    #[command(name = "__recover", hide = true)]
    Recover(crate::recovery::RecoveryArgs),
}

/// Everything a caller can say about how to apply a package.
#[derive(Debug, Clone, Default, Args)]
pub struct LifecycleArgs {
    /// Which installation this applies to.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    /// Add a component, repeatable.
    #[arg(long = "enable", alias = "component", value_name = "ID")]
    pub enable: Vec<String>,
    /// Remove a component, repeatable.
    #[arg(long = "disable", value_name = "ID")]
    pub disable: Vec<String>,
    /// Install somewhere other than the default, where the application allows it.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Option<PathBuf>,
    /// Answer with a machine-readable result instead of prose.
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    /// Proceed without asking for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// The state root to read and write. Derived from the scope when absent.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// The scratch directory for a transaction. Derived when absent.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
    /// A local release tree to read before the network.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub source: Option<PathBuf>,
    /// The graphical surface, where this build has one.
    #[arg(long, hide = true)]
    pub ui: bool,
    /// Read payload from a verified content cache rather than from this image.
    ///
    /// This is how a dispatcher hands over: it resolved an authenticated release,
    /// filled a cache, and verified the runtime it is now running as. The cache is
    /// a location, not an authority - every blob still has to hash to a digest the
    /// authenticated release named.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub acquired: Option<PathBuf>,
    /// The handoff document a dispatcher wrote, naming the release to install.
    #[arg(long, hide = true, value_hint = ValueHint::FilePath)]
    pub handoff: Option<PathBuf>,
    /// The digest of `handoff`, as the launcher computed it.
    #[arg(long, hide = true)]
    pub handoff_digest: Option<String>,
}

impl LifecycleArgs {
    /// The output format the caller asked for.
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }

    /// Whether this invocation may ask the person in front of it a question.
    ///
    /// Only the console frontend, on a terminal, in prose, and only when the
    /// caller did not already say `--yes` or `--non-interactive`. Machine output
    /// and the headless frontend never prompt: a question nobody can answer is a
    /// process that never returns.
    pub fn may_prompt(&self, context: RuntimeContext) -> bool {
        !self.non_interactive
            && !self.yes
            && context.output == OutputFormat::Human
            && context.is_live_console()
    }
}

/// `repair` takes no selection: it restores what was committed.
#[derive(Debug, Args)]
pub struct RepairArgs {
    #[command(flatten)]
    pub install: LifecycleArgs,
    /// Replace a file whose contents differ from the committed digest, rather
    /// than only restoring the ones that are missing.
    #[arg(long)]
    pub force_files: bool,
}

/// The out-of-process uninstall runner.
#[derive(Debug, Args)]
pub(crate) struct UninstallRunnerArgs {
    /// The process to wait for before touching the file it is running from.
    #[arg(long)]
    wait_pid: u32,
    #[command(flatten)]
    uninstall: crate::uninstall::UninstallArgs,
}

/// Which installation an operation applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    #[default]
    User,
    Machine,
    Either,
}

impl ScopeArg {
    /// The wire name, for a command line this process builds for its own child.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Machine => "machine",
            Self::Either => "either",
        }
    }
}

impl From<ScopeArg> for zup_core::SelectedScope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::User => Self::User,
            ScopeArg::Machine => Self::Machine,
            ScopeArg::Either => Self::User,
        }
    }
}

/// The format a result is written in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    #[default]
    Human,
    Json,
    Jsonl,
}

impl OutputArg {
    /// The wire name, for a command line this process builds for its own child.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
            Self::Jsonl => "jsonl",
        }
    }
}

impl From<OutputArg> for OutputFormat {
    fn from(value: OutputArg) -> Self {
        match value {
            OutputArg::Human => Self::Human,
            OutputArg::Json => Self::Json,
            OutputArg::Jsonl => Self::Jsonl,
        }
    }
}

impl Cli {
    /// The output format the invocation asked for.
    pub(crate) fn output_format(&self) -> OutputFormat {
        match &self.command {
            Some(Commands::Install(args))
            | Some(Commands::Modify(args))
            | Some(Commands::Upgrade(args)) => args.output(),
            Some(Commands::Repair(args)) => args.install.output(),
            Some(Commands::Update(args)) => args.output(),
            Some(Commands::Uninstall(args)) => args.output(),
            Some(Commands::UninstallRunner(args)) => args.uninstall.output(),
            Some(Commands::Recover(args)) => args.output.into(),
            _ => OutputFormat::Human,
        }
    }
}
