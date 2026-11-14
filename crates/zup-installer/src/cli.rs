use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueHint};

use zup_presentation::OutputFormat;

use crate::lifecycle;
use crate::run::RuntimeContext;
use crate::terminal::{Output, Scope};

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

pub fn dispatch(cli: Cli, context: RuntimeContext) -> miette::Result<()> {
    match cli.command {
        Some(Commands::Inspect(args)) => crate::terminal::inspect(
            &mut crate::host::WindowsSessionHost::default(),
            args.scope.map(Into::into),
            args.state_root.as_deref(),
            args.output.into(),
        ),
        Some(Commands::Cleanup(args)) => lifecycle::run(
            context,
            lifecycle::Verb::Cleanup { keep: args.keep },
            args.install,
        ),
        Some(Commands::Install(args)) => lifecycle::apply(context, args),
        Some(Commands::Downgrade(args)) => {
            lifecycle::run(context, lifecycle::Verb::Downgrade, args)
        }
        Some(Commands::Rollback(args)) => lifecycle::run(
            context,
            lifecycle::Verb::Rollback {
                revision: args.revision,
            },
            args.install,
        ),
        Some(Commands::Modify(args)) => lifecycle::run(context, lifecycle::Verb::Modify, args),
        Some(Commands::Repair(args)) => lifecycle::run(
            context,
            lifecycle::Verb::Repair { force: args.force },
            args.install,
        ),
        Some(Commands::Update(args)) => crate::maintenance::run_update(context, args),
        Some(Commands::Uninstall(args)) => crate::maintenance::run_uninstall(context, args),
        Some(Commands::Worker { bootstrap }) => crate::run::run_worker(&bootstrap),
        Some(Commands::Upgrade(args)) => lifecycle::run(context, lifecycle::Verb::Upgrade, args),
        Some(Commands::Recover(args)) => crate::maintenance::run_recovery(context, args),
        None => lifecycle::direct_launch(context),
    }
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    Cleanup(CleanupArgs),
    Inspect(InspectArgs),
    Install(LifecycleArgs),
    Modify(LifecycleArgs),
    Downgrade(LifecycleArgs),
    Rollback(RollbackArgs),
    Repair(RepairArgs),
    Update(crate::maintenance::UpdateArgs),
    Uninstall(crate::maintenance::UninstallArgs),
    #[command(name = "__worker", hide = true)]
    Worker {
        bootstrap: String,
    },
    Upgrade(LifecycleArgs),
    Recover(crate::maintenance::RecoveryArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, value_hint = ValueHint::DirPath, hide = true)]
    pub state_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub output: Output,
}

#[derive(Debug, Clone, Default, Args)]
pub struct LifecycleArgs {
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    #[arg(long = "enable", alias = "component", value_name = "ID")]
    pub enable: Vec<String>,
    #[arg(long = "disable", value_name = "ID")]
    pub disable: Vec<String>,
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Option<PathBuf>,
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
}

#[derive(Debug, Args)]
pub struct CleanupArgs {
    #[command(flatten)]
    pub install: LifecycleArgs,
    #[arg(long, default_value_t = 1)]
    pub keep: u16,
}

#[derive(Debug, Args)]
pub struct RollbackArgs {
    #[command(flatten)]
    pub install: LifecycleArgs,
    #[arg(long)]
    pub revision: zup_core::ActivationRevision,
}

#[derive(Debug, Args)]
pub struct RepairArgs {
    #[command(flatten)]
    pub install: LifecycleArgs,
    #[arg(long)]
    pub force: bool,
}

impl Cli {
    pub(crate) fn may_show_error_dialog(&self) -> bool {
        if self.output_format() != OutputFormat::Human {
            return false;
        }
        match &self.command {
            None => true,
            Some(
                Commands::Install(args)
                | Commands::Modify(args)
                | Commands::Upgrade(args)
                | Commands::Downgrade(args),
            ) => args.ui,
            Some(Commands::Repair(args)) => args.install.ui,
            Some(Commands::Cleanup(args)) => args.install.ui,
            Some(Commands::Rollback(args)) => args.install.ui,
            Some(Commands::Uninstall(args)) => args.ui,
            Some(
                Commands::Inspect(_)
                | Commands::Update(_)
                | Commands::Worker { .. }
                | Commands::Recover(_),
            ) => false,
        }
    }

    pub(crate) fn output_format(&self) -> OutputFormat {
        match &self.command {
            Some(Commands::Install(args))
            | Some(Commands::Modify(args))
            | Some(Commands::Upgrade(args))
            | Some(Commands::Downgrade(args)) => args.output(),
            Some(Commands::Repair(args)) => args.install.output(),
            Some(Commands::Cleanup(args)) => args.install.output(),
            Some(Commands::Rollback(args)) => args.install.output(),
            Some(Commands::Inspect(args)) => args.output.into(),
            Some(Commands::Update(args)) => args.output(),
            Some(Commands::Uninstall(args)) => args.output(),
            Some(Commands::Recover(args)) => args.output.into(),
            _ => OutputFormat::Human,
        }
    }
}
