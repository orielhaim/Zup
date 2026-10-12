use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};

use zup_presentation::OutputFormat;

use crate::frontend;
use crate::lifecycle;
use crate::run::RuntimeContext;

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

fn command_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(invoked_name).as_str()
}

pub fn parser() -> clap::Command {
    Cli::command()
        .name(command_name())
        .about("Install and maintain this application.")
}

pub fn parse() -> Cli {
    let matches = parser().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

#[cfg(test)]
pub fn public_commands() -> Vec<String> {
    parser()
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect()
}

#[cfg(test)]
pub fn internal_commands() -> Vec<String> {
    parser()
        .get_subcommands()
        .filter(|command| command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect()
}

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
        Some(Commands::Update(args)) => crate::maintenance::run_update(context, args),
        Some(Commands::Uninstall(args)) => crate::maintenance::run_uninstall(context, args),
        Some(Commands::Worker { bootstrap }) => crate::run::run_worker(&bootstrap),
        Some(Commands::UninstallRunner(args)) => {
            zup_windows::wait_for_process_exit(args.wait_pid)
                .map_err(|error| miette::miette!("wait for maintenance process: {error}"))?;
            let cleanup =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            let result = if args.uninstall.ui {
                uninstall_confirmation(context, &args.uninstall)
            } else {
                crate::maintenance::run_uninstall(context, args.uninstall)
            };
            crate::maintenance::schedule_runner_cleanup(&cleanup);
            result
        }
        Some(Commands::Upgrade(args)) => lifecycle::run(context, lifecycle::Verb::Upgrade, args),
        Some(Commands::Recover(args)) => crate::maintenance::run_recovery(context, args),
        None => lifecycle::direct_launch(context),
    }
}

fn uninstall_confirmation(
    context: RuntimeContext,
    args: &crate::maintenance::UninstallArgs,
) -> miette::Result<()> {
    let executable =
        zup_windows::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(&executable)
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    frontend::graphical_uninstall(context, &executable, &bundle, args)
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    Install(LifecycleArgs),
    Modify(LifecycleArgs),
    Repair(RepairArgs),
    Update(crate::maintenance::UpdateArgs),
    Uninstall(crate::maintenance::UninstallArgs),
    #[command(name = "__worker", hide = true)]
    Worker {
        bootstrap: String,
    },
    #[command(name = "__uninstall_runner", hide = true)]
    UninstallRunner(UninstallRunnerArgs),
    #[command(name = "__upgrade", hide = true)]
    Upgrade(LifecycleArgs),
    #[command(name = "__recover", hide = true)]
    Recover(crate::maintenance::RecoveryArgs),
}

#[derive(Debug, Clone, Default, Args)]
pub struct LifecycleArgs {
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    #[arg(long = "enable", alias = "component", value_name = "ID")]
    pub enable: Vec<String>,
    #[arg(long = "disable", value_name = "ID")]
    pub disable: Vec<String>,
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub yes: bool,

    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub source: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub ui: bool,
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub acquired: Option<PathBuf>,
    #[arg(long, hide = true, value_hint = ValueHint::FilePath)]
    pub handoff: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub handoff_digest: Option<String>,
}

impl LifecycleArgs {
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }

    /// and the headless frontend never prompt: a question nobody can answer is a
    /// process that never returns.
    pub fn may_prompt(&self, context: RuntimeContext) -> bool {
        !self.non_interactive
            && !self.yes
            && context.output == OutputFormat::Human
            && context.is_live_console()
    }
}

#[derive(Debug, Args)]
pub struct RepairArgs {
    #[command(flatten)]
    pub install: LifecycleArgs,
    #[arg(long)]
    pub force_files: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UninstallRunnerArgs {
    #[arg(long)]
    wait_pid: u32,
    #[command(flatten)]
    uninstall: crate::maintenance::UninstallArgs,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    #[default]
    User,
    Machine,
    Either,
}

impl ScopeArg {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    #[default]
    Human,
    Json,
    Jsonl,
}

impl OutputArg {
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
