use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};
use zup_automation::AutomationResult;
use zup_core::Frontend;
use zup_presentation::OutputFormat;

use crate::failure::Reporter;
use crate::project::TargetOverrideArgs;
use crate::toolchain::ToolchainVerb;

const ABOUT: &str = "Build and distribute zup installers";

#[derive(Debug, Parser)]
#[command(name = "zup", version, about = ABOUT, disable_help_subcommand = true)]
pub struct Cli {
    #[arg(long, global = true, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub toolchain: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

pub fn parser() -> clap::Command {
    Cli::command()
}

pub fn parse() -> Cli {
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match parser().try_get_matches_from(&arguments) {
        Ok(matches) => Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit()),
        Err(error) => {
            if parse_error(&error, arguments).is_none() {
                error.exit();
            }
            std::process::exit(process_exit_code_for_invocation(&error));
        }
    }
}

pub fn dispatch(cli: Cli) -> miette::Result<()> {
    let toolchain = cli.toolchain;
    match cli.command {
        Some(Commands::Init(args)) => crate::init::run(args),
        Some(Commands::Check(args)) => {
            operation(args.format, zup_automation::OPERATION_CHECK, || {
                crate::check::run_check(args, toolchain)
            })
        }
        Some(Commands::Doctor(args)) => {
            operation(args.format, zup_automation::OPERATION_DOCTOR, || {
                crate::doctor::run(args, toolchain)
            })
        }
        Some(Commands::Plan(args)) => {
            operation(args.format, zup_automation::OPERATION_PLAN, || {
                crate::check::run_plan(args, toolchain)
            })
        }
        Some(Commands::Preview(args)) => crate::preview::run(args, toolchain),
        Some(Commands::Build(args)) => {
            operation(args.format, zup_automation::OPERATION_BUILD, || {
                crate::build::run(args, toolchain)
            })
        }
        Some(Commands::Sign(args)) => {
            let root = args.location.release_dir;
            match args.command {
                SignVerb::Prepare(args) => {
                    operation(args.format, zup_automation::OPERATION_SIGN_PREPARE, || {
                        crate::signing::run_prepare(root, args)
                    })
                }
                SignVerb::Verify(args) => {
                    operation(args.format, zup_automation::OPERATION_SIGN_VERIFY, || {
                        crate::signing::run_verify(root, args)
                    })
                }
            }
        }
        Some(Commands::Artifact(args)) => match args.command {
            ArtifactVerb::Inspect(args) => operation(
                args.format,
                zup_automation::OPERATION_ARTIFACT_INSPECT,
                || crate::inspect_artifact::run(args),
            ),
        },
        Some(Commands::Publish(args)) => match args.command {
            PublishVerb::Stage(args) => {
                operation(args.format, zup_automation::OPERATION_PUBLISH_STAGE, || {
                    crate::publish::run_stage(args, toolchain)
                })
            }
            PublishVerb::Github(args) => operation(
                args.format,
                zup_automation::OPERATION_PUBLISH_GITHUB,
                || crate::publish::run_github(args),
            ),
        },
        Some(Commands::Toolchain(args)) => match args.command {
            ToolchainVerb::Install(args) => operation(
                args.format,
                zup_automation::OPERATION_TOOLCHAIN_INSTALL,
                || crate::toolchain::run_install(args, toolchain),
            ),
            ToolchainVerb::Status(args) => operation(
                args.format,
                zup_automation::OPERATION_TOOLCHAIN_STATUS,
                || crate::toolchain::run_status(args, toolchain),
            ),
            ToolchainVerb::Clean(args) => operation(
                args.format,
                zup_automation::OPERATION_TOOLCHAIN_CLEAN,
                || crate::toolchain::run_clean(args),
            ),
        },
        Some(Commands::Preset(args)) => match args.command {
            crate::preset::PresetVerb::Init(args) => crate::preset::generate(&args),
            crate::preset::PresetVerb::Dev(args) => crate::preset::dev(&args),
            crate::preset::PresetVerb::Pack(args) => crate::preset::pack(&args),
            crate::preset::PresetVerb::Inspect(args) => crate::preset::inspect(&args),
        },
        Some(Commands::Plugin(args)) => match args.command {
            crate::plugin::PluginVerb::Init(args) => crate::plugin::init(&args),
            crate::plugin::PluginVerb::Build(args) => crate::plugin::build(&args),
        },
        Some(Commands::Ci(args)) => crate::ci::run(args),
        Some(Commands::Schema(args)) => crate::manifest_tools::run_schema(args),
        Some(Commands::Fmt(args)) => crate::manifest_tools::run_fmt(args),
        Some(Commands::Completions(args)) => crate::manifest_tools::run_completions(args),
        None => {
            print!("{}", parser().render_help());
            Ok(())
        }
    }
}

/// never disagree: a failure writes a failure result *and* returns the error `main`
fn operation(
    format: OutputArg,
    name: &'static str,
    body: impl FnOnce() -> miette::Result<AutomationResult>,
) -> miette::Result<()> {
    let reporter = Reporter::new(format);
    reporter.begin(name);
    let result = match body() {
        Ok(result) => result,
        Err(error) => {
            reporter.finish(
                AutomationResult::new(name)
                    .failed()
                    .with_diagnostic(crate::failure::failure(name, &error, "")),
            );
            return Err(error);
        }
    };
    if let Err(error) = result.validate() {
        reporter.finish(AutomationResult::new(name).failed().with_diagnostic(
            zup_automation::Diagnostic::error("zup.internal.invalid_result", error.to_string()),
        ));
        return Err(miette::miette!(
            "{name} produced a result that does not satisfy the protocol"
        ));
    }
    let failed = result.status == zup_automation::Status::Failure;
    reporter.finish(result.clone());
    if failed {
        return Err(refusal(&result));
    }
    Ok(())
}

fn refusal(result: &AutomationResult) -> miette::Report {
    let detail = result
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let summary = result
        .summary
        .clone()
        .unwrap_or_else(|| format!("`{}` failed", result.operation));
    let message = if detail.is_empty() {
        summary
    } else {
        format!("{summary}: {detail}")
    };
    let code = result.diagnostics.first().map_or(
        zup_automation::Identifier::fixed(zup_automation::FALLBACK_CODE),
        |d| d.code.clone(),
    );
    let report = crate::failure::identified(code, message);
    for diagnostic in &result.diagnostics {
        if let Some(help) = &diagnostic.help {
            return report.wrap_err(help.clone());
        }
    }
    report
}

fn parse_error(error: &clap::Error, arguments: Vec<std::ffi::OsString>) -> Option<()> {
    let format = requested_format(&arguments)?;
    let operation = requested_operation(&arguments)?;
    let reporter = Reporter::new(format);
    reporter.begin(operation);
    let diagnostic = zup_automation::Diagnostic::error(
        "zup.cli.invalid_invocation",
        error.render().to_string().trim().to_owned(),
    )
    .with_help(
        "Run `zup --help`, or `zup <command> --help`, for the arguments this command accepts.",
    );
    reporter.finish(
        AutomationResult::new(operation)
            .failed()
            .with_diagnostic(diagnostic),
    );
    Some(())
}

pub fn process_exit_code_for_invocation(error: &clap::Error) -> i32 {
    use clap::error::ErrorKind;
    match error.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
        _ => 3,
    }
}

fn requested_format(arguments: &[std::ffi::OsString]) -> Option<OutputArg> {
    let mut words = arguments
        .iter()
        .skip(1)
        .map(|argument| argument.to_string_lossy());
    while let Some(argument) = words.next() {
        let inline = argument.strip_prefix("--format=");
        let value: &str = match inline {
            Some(value) => value,
            None if argument == "--format" => &words.next()?,
            None => continue,
        };
        return value.parse::<OutputArg>().ok();
    }
    None
}

fn requested_operation(arguments: &[std::ffi::OsString]) -> Option<&'static str> {
    const VERBS: &[&str] = &[
        "check",
        "doctor",
        "plan",
        "build",
        "sign",
        "artifact",
        "publish",
        "toolchain",
    ];
    arguments.iter().skip(1).find_map(|argument| {
        let word = argument.to_string_lossy();
        VERBS.iter().copied().find(|verb| *verb == word)
    })
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Init(InitCommand),
    Check(CheckCommand),
    Doctor(crate::doctor::DoctorCommand),
    Plan(PlanCommand),
    Preview(crate::preview::PreviewCommand),
    Build(BuildCommand),
    Artifact(ArtifactCommand),
    Sign(SignCommand),
    Publish(PublishCommand),
    Ci(crate::ci::CiCommand),
    Toolchain(crate::toolchain::ToolchainCommand),
    Preset(crate::preset::PresetCommand),
    Plugin(crate::plugin::PluginCommand),
    Schema(SchemaCommand),
    Fmt(FmtCommand),
    Completions(CompletionsCommand),
}

#[derive(Debug, Args)]
pub struct InitCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub app_id: Option<String>,
    #[arg(long, default_value = "0.1.0")]
    pub version: String,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Option<String>,
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
    #[arg(long)]
    pub main: Option<String>,
    #[arg(long)]
    pub force: bool,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Debug, Args)]
pub struct CheckCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

#[derive(Debug, Args)]
pub struct PlanCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long = "enable", value_name = "ID")]
    pub enable: Vec<String>,
    #[arg(long = "disable", value_name = "ID")]
    pub disable: Vec<String>,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

#[derive(Debug, Args, Clone)]
pub struct ProjectSelection {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
}

impl ProjectSelection {
    pub fn overrides(&self) -> TargetOverrideArgs {
        TargetOverrideArgs {
            source: self.source.clone(),
            install_directory: self.install_directory.clone(),
            frontend: self.frontend.map(Frontend::from),
        }
    }

    pub fn bare(manifest: PathBuf, target: Vec<String>) -> Self {
        Self {
            manifest,
            target,
            source: Vec::new(),
            install_directory: Vec::new(),
            frontend: None,
        }
    }
}

impl Default for ProjectSelection {
    fn default() -> Self {
        Self::bare(PathBuf::from(crate::DEFAULT_MANIFEST), Vec::new())
    }
}

#[derive(Debug, Args)]
pub struct BuildCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Vec<PathBuf>,
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub runtime: Vec<PathBuf>,
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub dispatcher: Vec<PathBuf>,
    #[arg(long)]
    pub force: bool,
    #[arg(long, value_name = "ARTIFACT", conflicts_with = "universal")]
    pub artifact: Vec<String>,
    #[arg(long, conflicts_with = "artifact")]
    pub universal: bool,
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath, default_value = "zup-release.json")]
    pub release_manifest: String,
    /// checked by `zup sign verify`. It is a *name*, not a key: zup never holds the
    #[arg(long, value_name = "SUBJECT")]
    pub signing_subject: Option<String>,
}

#[derive(Debug, Args)]
pub struct ArtifactCommand {
    #[command(subcommand)]
    pub command: ArtifactVerb,
}

#[derive(Debug, Args, Clone)]
pub struct ReleaseLocation {
    #[arg(long, value_name = "DIR", default_value = "dist", value_hint = ValueHint::DirPath)]
    pub release_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct SignCommand {
    #[command(flatten)]
    pub location: ReleaseLocation,
    #[command(subcommand)]
    pub command: SignVerb,
}

#[derive(Debug, Subcommand)]
pub enum SignVerb {
    Prepare(SignPrepareCommand),
    Verify(SignVerifyCommand),
}

#[derive(Debug, Args)]
pub struct SignPrepareCommand {
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long, value_name = "SUBJECT")]
    pub subject: Option<String>,
    #[arg(long, value_name = "THUMBPRINT")]
    pub thumbprint: Option<String>,
    #[arg(long)]
    pub allow_untrusted_chain: bool,
    #[arg(long)]
    pub allow_missing_timestamp: bool,
}

#[derive(Debug, Args)]
pub struct SignVerifyCommand {
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long)]
    pub report_only: bool,
    #[arg(long)]
    pub allow_unsigned: bool,
    /// Off by default: a build host without access to a CRL must not fail an
    #[arg(long)]
    pub online_revocation: bool,
}

#[derive(Debug, Subcommand)]
pub enum ArtifactVerb {
    Inspect(ArtifactInspectCommand),
}

#[derive(Debug, Args)]
pub struct ArtifactInspectCommand {
    #[arg(value_name = "ARTIFACT", value_hint = ValueHint::FilePath)]
    pub artifact: PathBuf,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

#[derive(Debug, Args)]
pub struct PublishCommand {
    #[command(subcommand)]
    pub command: PublishVerb,
}

#[derive(Debug, Subcommand)]
pub enum PublishVerb {
    Stage(PublishStageCommand),
    Github(PublishGithubCommand),
}

#[derive(Debug, Args)]
pub struct PublishStageCommand {
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long, default_value = "dist/web", value_hint = ValueHint::DirPath)]
    pub output: PathBuf,
    #[arg(long, default_value = "stable")]
    pub channel: String,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    #[arg(long)]
    pub thin: bool,
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub dispatcher: Option<PathBuf>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub thin_output: Option<PathBuf>,
    #[arg(long, value_hint = ValueHint::Url)]
    pub repository: Option<String>,
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath)]
    pub download: Vec<PathBuf>,
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub packages: Option<PathBuf>,
    /// installer is never split.
    #[arg(long, value_name = "BYTES")]
    pub shard_bytes: Option<u64>,
    #[arg(long, value_name = "DIR", value_hint = ValueHint::FilePath)]
    pub release_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PublishGithubCommand {
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long, default_value = "dist", value_hint = ValueHint::DirPath)]
    pub release_dir: PathBuf,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub web: Option<PathBuf>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub packages: Option<PathBuf>,
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub draft: bool,
    #[arg(long)]
    pub prerelease: bool,
    #[arg(long)]
    pub dry_run: bool,
    /// Never applies to a published release: those bytes are already public and
    #[arg(long)]
    pub replace_conflicts: bool,
    #[arg(long, value_name = "TEXT")]
    pub notes_text: Option<String>,
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub receipt: Option<PathBuf>,
}

#[derive(Debug, Args, Default)]
pub struct SchemaCommand {
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FmtCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsCommand {
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    /// Prose for a person. Never parsed, and free to change.
    #[default]
    Human,
    Json,
    Jsonl,
}

impl std::str::FromStr for OutputArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "human" => Ok(Self::Human),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            other => Err(format!(
                "unknown output format `{other}`; expected human, json or jsonl"
            )),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    User,
    Machine,
    Either,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FrontendArg {
    Gui,
    Console,
    Headless,
}

impl From<FrontendArg> for Frontend {
    fn from(value: FrontendArg) -> Self {
        match value {
            FrontendArg::Gui => Self::Gui,
            FrontendArg::Console => Self::Console,
            FrontendArg::Headless => Self::Headless,
        }
    }
}
