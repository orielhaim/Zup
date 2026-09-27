//! The developer CLI's command surface.
//!
//! Every verb here is one a person does *to a project*: create one, check it,
//! plan it, build it, inspect what it produced, publish it, wire it into CI,
//! format it, generate completions for it. None of them is something a person does
//! *to an installed application*, because the executable that does those things
//! is a different one and lives on the user's own machine.
//!
//! The `about` text is the first line anybody reads of this tool, so it says what
//! the tool is rather than what technology it is made of.

use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};
use zup_core::Frontend;
use zup_presentation::OutputFormat;

use crate::project::TargetOverrideArgs;

/// The description shown at the top of `zup --help`.
const ABOUT: &str = "Build and distribute zup installers";

/// The developer tool.
#[derive(Debug, Parser)]
#[command(name = "zup", version, about = ABOUT, disable_help_subcommand = true)]
pub struct Cli {
    /// Read zup's own binaries — runtime templates, dispatchers — from this
    /// directory instead of the ones installed for this zup release.
    #[arg(long, global = true, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub toolchain: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// The developer CLI's parser.
pub fn parser() -> clap::Command {
    Cli::command()
}

/// Parse `argv`, exiting the way a CLI exits on `--help` or a usage error.
pub fn parse() -> Cli {
    let matches = parser().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// Run one command.
pub fn dispatch(cli: Cli) -> miette::Result<()> {
    match cli.command {
        Some(Commands::Init(args)) => crate::init::run(args),
        Some(Commands::Check(args)) => crate::check::run_check(args),
        Some(Commands::Doctor(args)) => crate::doctor::run(args, cli.toolchain),
        Some(Commands::Plan(args)) => crate::check::run_plan(args),
        Some(Commands::Build(args)) => crate::build::run(args, cli.toolchain),
        Some(Commands::Artifact(args)) => match args.command {
            ArtifactVerb::Inspect(args) => crate::inspect_artifact::run(args),
        },
        Some(Commands::Publish(args)) => match args.command {
            PublishVerb::Stage(args) => crate::publish::run_stage(args, cli.toolchain),
            PublishVerb::Github(args) => crate::publish::run_github(args),
        },
        Some(Commands::Ci(args)) => crate::ci::run(args),
        Some(Commands::Schema(args)) => crate::manifest_tools::run_schema(args),
        Some(Commands::Fmt(args)) => crate::manifest_tools::run_fmt(args),
        Some(Commands::Completions(args)) => crate::manifest_tools::run_completions(args),
        // A tool with no verb prints what it can do. It does not go looking for an
        // installer package: that is the generated executable's job, and a
        // developer running `zup` in a project directory wants a usage line.
        None => {
            print!("{}", parser().render_help());
            Ok(())
        }
    }
}

/// Every authoring and distribution operation.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a small, editable project.
    Init(InitCommand),
    /// Validate a project and the files it will ship.
    Check(CheckCommand),
    /// Report whether this project is ready to build.
    Doctor(crate::doctor::DoctorCommand),
    /// Show what installing this project would do, without changing anything.
    Plan(PlanCommand),
    /// Build the configured distribution artifacts.
    Build(BuildCommand),
    /// Look at what a build produced.
    Artifact(ArtifactCommand),
    /// Publish a release.
    Publish(PublishCommand),
    /// Generate and check the release pipeline a project commits.
    Ci(crate::ci::CiCommand),
    /// Print the authoritative zup.toml JSON Schema.
    Schema(SchemaCommand),
    /// Format zup.toml without losing its comments.
    Fmt(FmtCommand),
    /// Generate shell completions for zup.
    Completions(CompletionsCommand),
}

/// Create a small, editable project.
#[derive(Debug, Args)]
pub struct InitCommand {
    /// The manifest to create.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// The application's display name.
    #[arg(long)]
    pub name: Option<String>,
    /// The application's reverse-DNS identifier.
    #[arg(long)]
    pub app_id: Option<String>,
    /// The version to start at.
    #[arg(long, default_value = "0.1.0")]
    pub version: String,
    /// The directory the payload lives in.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Option<String>,
    /// Who the application installs for.
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    /// The experience the installer presents.
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
    /// The file Windows launches.
    #[arg(long)]
    pub main: Option<String>,
    /// Replace a manifest that already exists.
    #[arg(long)]
    pub force: bool,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
}

/// Validate a project and the files it will ship.
#[derive(Debug, Args)]
pub struct CheckCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
}

/// Show what installing this project would do.
#[derive(Debug, Args)]
pub struct PlanCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
    /// Who the plan is for.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    /// The state root to read the existing installation from.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// Add a component to the plan.
    #[arg(long = "enable", value_name = "ID")]
    pub enable: Vec<String>,
    /// Remove a component from the plan.
    #[arg(long = "disable", value_name = "ID")]
    pub disable: Vec<String>,
    /// Write the plan as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Which manifest, and which of its target profiles.
#[derive(Debug, Args, Clone)]
pub struct ProjectSelection {
    /// The manifest to read.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// A target profile name or triple, repeatable. Empty selects every profile.
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    /// Build source directory for each selected target, relative to the project.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    /// Default install directory for each selected target.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    /// The frontend for each selected target.
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
}

impl ProjectSelection {
    /// The per-profile overrides this selection carries.
    pub fn overrides(&self) -> TargetOverrideArgs {
        TargetOverrideArgs {
            source: self.source.clone(),
            install_directory: self.install_directory.clone(),
            frontend: self.frontend.map(Frontend::from),
        }
    }

    /// The same selection with no overrides, for a command that takes none.
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

/// Build the configured distribution artifacts.
#[derive(Debug, Args)]
pub struct BuildCommand {
    #[command(flatten)]
    pub project: ProjectSelection,
    /// Where to write the artifacts, one per artifact or per target.
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Vec<PathBuf>,
    /// A runtime template to use instead of the one this zup release provides.
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub runtime: Vec<PathBuf>,
    /// A dispatcher to use instead of the one this zup release provides.
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub dispatcher: Vec<PathBuf>,
    /// Overwrite an output that already exists instead of refusing to.
    #[arg(long)]
    pub force: bool,
    /// Build one declared distribution artifact, repeatable.
    ///
    /// Semantically distinct from `--target`: `--target` names a native variant to
    /// build, `--artifact` names a file a user downloads.
    #[arg(long, value_name = "ARTIFACT", conflicts_with = "universal")]
    pub artifact: Vec<String>,
    /// Compose one universal artifact from every selected target.
    ///
    /// Refuses rather than guessing when the selected targets cannot form one
    /// artifact.
    #[arg(long, conflicts_with = "artifact")]
    pub universal: bool,
    /// Where to write the machine-readable release description, or `none` to
    /// skip it. Defaults to `dist/zup-release.json` beside the manifest.
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath)]
    pub release_manifest: Option<String>,
}

/// Look at what a build produced.
#[derive(Debug, Args)]
pub struct ArtifactCommand {
    #[command(subcommand)]
    pub command: ArtifactVerb,
}

/// What to do with an artifact.
#[derive(Debug, Subcommand)]
pub enum ArtifactVerb {
    /// Describe what an artifact contains and how it verifies.
    Inspect(ArtifactInspectCommand),
}

/// Describe a built artifact.
#[derive(Debug, Args)]
pub struct ArtifactInspectCommand {
    /// The artifact to inspect.
    #[arg(value_name = "ARTIFACT", value_hint = ValueHint::FilePath)]
    pub artifact: PathBuf,
    /// Readable text or the versioned JSON report.
    #[arg(long, value_enum, default_value = "human")]
    pub format: FormatArg,
}

/// Publish a release.
#[derive(Debug, Args)]
pub struct PublishCommand {
    #[command(subcommand)]
    pub command: PublishVerb,
}

/// The publishing steps zup performs.
///
/// Signing is not one of them. Keys live outside zup and the TUF metadata is
/// produced by `tuftool`, so a release is staged as a directory of files and
/// signed by the tool that already knows how to sign a TUF repository.
#[derive(Debug, Subcommand)]
pub enum PublishVerb {
    /// Write the web tree a static origin serves and a TUF repository signs.
    Stage(PublishStageCommand),
    /// Publish the staged release to GitHub.
    Github(PublishGithubCommand),
}

/// Stage everything a static origin serves and a TUF repository signs.
#[derive(Debug, Args)]
pub struct PublishStageCommand {
    /// The manifest to read.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// The directory the web tree is written to.
    #[arg(long, default_value = "dist/web", value_hint = ValueHint::DirPath)]
    pub output: PathBuf,
    /// The channel the staged release answers to.
    #[arg(long, default_value = "stable")]
    pub channel: String,
    /// Build source directory for each selected target, relative to the project.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    /// Default install directory for each selected target.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    /// The experience each selected target presents.
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendArg>,
    /// A target profile name or triple, repeatable.
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    /// Build the two thin installers as well as the web tree.
    #[arg(long)]
    pub thin: bool,
    /// A dispatcher to use instead of the one this zup release provides.
    #[arg(long, value_hint = ValueHint::FilePath, hide = true)]
    pub dispatcher: Option<PathBuf>,
    /// Where the thin installers are written.
    ///
    /// They are not part of the web tree: the tree is what a static origin
    /// serves, and an installer is what a person downloads. So they default to the
    /// directory beside it, and a publisher who wants them served alongside the
    /// graph says so.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub thin_output: Option<PathBuf>,
    /// The URL a published client reads the release graph from.
    ///
    /// Embedded in the thin installers, so it has to be the address the clients
    /// will use, not the path this build happens to write to. Defaults to the
    /// staged tree as a `file:` URL, which is right for a local origin and
    /// obviously wrong for a real one.
    #[arg(long, value_hint = ValueHint::Url)]
    pub repository: Option<String>,
    /// Finished installers to record as downloadable files, repeatable.
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath)]
    pub download: Vec<PathBuf>,
    /// Also write one transport package per variant.
    ///
    /// A package is not an installer: it is a container the acquisition engine
    /// reads, and it exists so that a project hosting its release on GitHub can
    /// carry its content in one or two assets per variant instead of one per
    /// object. Nothing reads them unless a distribution host is configured for it.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub packages: Option<PathBuf>,
    /// The largest a transport package may be before it is split.
    ///
    /// Only relevant with `--packages`, and only for content packs: a user-facing
    /// installer is never split.
    #[arg(long, value_name = "BYTES")]
    pub shard_bytes: Option<u64>,
    /// Compose from a release another job already built.
    ///
    /// This is the local/global split made explicit: a build matrix produces
    /// per-target outputs on separate machines, and one compose job reads them
    /// back together.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::FilePath)]
    pub release_dir: Option<PathBuf>,
}

/// Publish a release to GitHub.
///
/// Everything a developer normally has to type is derived: the repository from
/// the remote or `GITHUB_REPOSITORY`, the tag from the version, the asset list
/// from `zup-release.json` and the staged tree, and the digests from the files.
/// The flags are for the cases where the derivation is wrong, and none of them is
/// needed for the ordinary one.
#[derive(Debug, Args)]
pub struct PublishGithubCommand {
    /// The manifest to read.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// The directory a build wrote its release description into.
    #[arg(long, default_value = "dist", value_hint = ValueHint::DirPath)]
    pub release_dir: PathBuf,
    /// The staged content tree, for the documents a client authenticates.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub web: Option<PathBuf>,
    /// The directory holding the transport packages, for GitHub-hosted content.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub packages: Option<PathBuf>,
    /// `owner/name`, or `host/owner/name` for GitHub Enterprise.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,
    /// The release tag. Derived from the version as `v<version>`.
    #[arg(long)]
    pub tag: Option<String>,
    /// Leave the release a draft.
    #[arg(long)]
    pub draft: bool,
    /// Mark the release a prerelease.
    #[arg(long)]
    pub prerelease: bool,
    /// Plan and verify everything, and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Replace a differing asset on a draft release.
    ///
    /// Never applies to a published release: those bytes are already public and
    /// are what somebody downloaded.
    #[arg(long)]
    pub replace_conflicts: bool,
    /// The release body, when the project does not keep it in a file.
    #[arg(long, value_name = "TEXT")]
    pub notes_text: Option<String>,
    /// Where the provider's receipt is written.
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub receipt: Option<PathBuf>,
    /// Readable text or the versioned JSON report.
    #[arg(long, value_enum, default_value = "human")]
    pub format: GithubFormatArg,
}

/// Print the authoritative JSON Schema.
#[derive(Debug, Args, Default)]
pub struct SchemaCommand {
    /// Where to write it, or stdout when absent.
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
}

/// Format a manifest without losing its comments.
#[derive(Debug, Args)]
pub struct FmtCommand {
    /// The manifest to format.
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// Report whether it is formatted instead of writing.
    #[arg(long)]
    pub check: bool,
}

/// Generate shell completions for zup.
#[derive(Debug, Args)]
pub struct CompletionsCommand {
    /// The shell to generate for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Readable text or a versioned JSON report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    #[default]
    Human,
    Json,
}

/// Readable text or a versioned JSON report, for publishing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum GithubFormatArg {
    #[default]
    Human,
    Json,
}

/// Who an application installs for.
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

/// The experience an installer presents.
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

/// The format a machine-readable result is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    Human,
    Json,
    Jsonl,
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
