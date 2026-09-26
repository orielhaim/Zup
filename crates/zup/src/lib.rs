//! Developer-facing CLI for zup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[cfg(feature = "build")]
use clap::CommandFactory;
use clap::{Args, Parser, Subcommand, ValueEnum, ValueHint};
use std::io::IsTerminal;
use zup_bootstrap::{
    BootstrapId, BootstrapKey, BootstrapOperation, BootstrapPlan, BootstrapState,
    BootstrapStateStore, BoundBootstrapPlan, Quarantine,
};
#[cfg(feature = "build")]
use zup_core::TargetOverrides;
use zup_core::{
    AppId, ComponentId, Frontend, RelativePath, ResourceKey, SelectedScope, TargetTriple,
    hash_reader,
};
use zup_exec::LifecycleAction;
use zup_presentation::{AutomationEvent, AutomationResult, OutputFormat, ProcessOutcome};
use zup_runtime::{ExecutionPolicy, InstallOutcome, RuntimeRequest};

#[cfg(feature = "build")]
mod build_inputs;
#[cfg(feature = "build")]
pub mod doctor;

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
    /// Build a signed-ready installer package.
    #[cfg(feature = "build")]
    Build(BuildCommand),
    /// Create a small, editable zup.toml project.
    #[cfg(feature = "build")]
    Init(InitCommand),
    /// Validate a manifest and its build inputs.
    #[cfg(feature = "build")]
    Check(CheckCommand),
    /// Report whether the selected targets are ready to build.
    #[cfg(feature = "build")]
    Doctor(doctor::DoctorCommand),
    /// Inspect the real installation plan without changing the machine.
    #[cfg(feature = "build")]
    Plan(PlanCommand),
    /// Print or write the authoritative zup.toml JSON Schema.
    #[cfg(feature = "build")]
    Schema(SchemaCommand),
    /// Validate and format zup.toml while preserving comments.
    #[cfg(feature = "build")]
    Fmt(FmtCommand),
    /// Generate shell completions for zup.
    #[cfg(feature = "build")]
    Completions(CompletionsCommand),
    /// Install an embedded or source-manifest application.
    Install(ManifestCommand),
    /// Upgrade an installed application.
    Upgrade(ManifestCommand),
    /// Check for or install a verified update.
    Update(UpdateCommand),
    /// Change selected components for an installed application.
    Modify(ManifestCommand),
    /// Restore owned resources that have drifted.
    Repair(RepairCommand),
    /// Remove an installed application and its owned resources.
    Uninstall(UninstallCommand),
    #[command(name = "__uninstall_runner", hide = true)]
    UninstallRunner(UninstallRunnerCommand),
    /// Recover an interrupted transaction.
    Recover(RecoverCommand),
    #[command(name = "__worker", hide = true)]
    Worker { bootstrap: String },
    #[command(name = "__frontend", hide = true)]
    FrontendInfo,
    /// Print the protocol/worker bootstrap format for tests.
    #[command(hide = true)]
    WorkerHelp,
}

#[derive(Debug, Args)]
struct UpdateCommand {
    #[command(subcommand)]
    command: Option<UpdateCommands>,
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    output: OutputArg,
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum UpdateCommands {
    /// Check for an update without downloading it.
    Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScopeArg {
    User,
    Machine,
    Either,
}

impl From<ScopeArg> for SelectedScope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::User => Self::User,
            ScopeArg::Machine => Self::Machine,
            ScopeArg::Either => Self::User,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputArg {
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

fn effective_frontend() -> Frontend {
    match FRONTEND_OVERRIDE.load(Ordering::SeqCst) {
        1 => Frontend::Gui,
        2 => Frontend::Console,
        3 => Frontend::Headless,
        _ => compiled_frontend(),
    }
}

fn compiled_frontend() -> Frontend {
    #[cfg(feature = "gui")]
    {
        Frontend::Gui
    }
    #[cfg(all(not(feature = "gui"), feature = "console"))]
    {
        Frontend::Console
    }
    #[cfg(all(not(feature = "gui"), not(feature = "console")))]
    {
        Frontend::Headless
    }
}

fn default_install_scope(scope: zup_core::InstallScope) -> SelectedScope {
    match scope {
        zup_core::InstallScope::Machine => SelectedScope::Machine,
        zup_core::InstallScope::User | zup_core::InstallScope::Either => SelectedScope::User,
    }
}

#[cfg(any(feature = "gui", feature = "console"))]
fn resolve_interactive_action(
    requested: LifecycleAction,
    installed_version: Option<&semver::Version>,
    package_version: &semver::Version,
) -> miette::Result<LifecycleAction> {
    if requested != LifecycleAction::Install {
        return Ok(requested);
    }
    let Some(installed_version) = installed_version else {
        return Ok(LifecycleAction::Install);
    };
    match package_version.cmp(installed_version) {
        std::cmp::Ordering::Greater => Ok(LifecycleAction::Upgrade),
        std::cmp::Ordering::Equal => Ok(LifecycleAction::Modify),
        std::cmp::Ordering::Less => Err(miette::miette!(
            "downgrade from {installed_version} to {package_version} is refused"
        )),
    }
}

#[derive(Debug, Args)]
struct ManifestCommand {
    #[cfg(feature = "build")]
    #[arg(long, default_value = "zup.toml", value_hint = ValueHint::FilePath)]
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
    #[arg(long = "install-directory", alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    install_directory: Option<PathBuf>,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    target: Vec<String>,
    #[arg(long, hide = true)]
    ui: bool,
    #[arg(long, value_enum, default_value = "human")]
    output: OutputArg,
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    yes: bool,
    #[arg(long = "component")]
    component: Vec<String>,
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
    #[arg(long, hide = true)]
    ui: bool,
    #[arg(long)]
    app_id: Option<String>,
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    output: OutputArg,
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct UninstallRunnerCommand {
    #[arg(long)]
    wait_pid: u32,
    #[command(flatten)]
    uninstall: UninstallCommand,
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
    #[arg(long, value_enum, default_value = "human")]
    output: OutputArg,
    #[arg(long)]
    non_interactive: bool,
}

/// The manifest path every authoring command defaults to.
#[cfg(feature = "build")]
const DEFAULT_MANIFEST: &str = "zup.toml";

/// The `Default` of a command's argument struct is the invocation it performs
/// with no arguments, so the values clap applies as `default_value` are
/// repeated here rather than left to an empty `PathBuf`.
#[cfg(feature = "build")]
macro_rules! default_manifest {
    ($ty:ty { $($field:ident : $value:expr),* $(,)? }) => {
        impl Default for $ty {
            fn default() -> Self {
                Self { $($field: $value),* }
            }
        }
    };
}

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct BuildCommand {
    #[arg(long, default_value = DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long, value_hint = ValueHint::FilePath)]
    output: Vec<PathBuf>,
    #[arg(long, value_hint = ValueHint::FilePath)]
    runtime: Vec<PathBuf>,
    /// Build source directory for each selected target, relative to the project.
    #[arg(long, value_hint = ValueHint::DirPath)]
    source: Vec<PathBuf>,
    /// Default install directory the installer will use, for each selected target.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    install_directory: Vec<PathBuf>,
    #[arg(long, value_enum)]
    frontend: Option<FrontendArg>,
    /// Overwrite an output that already exists instead of refusing to.
    #[arg(long)]
    force: bool,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    target: Vec<String>,
}

#[cfg(feature = "build")]
default_manifest!(BuildCommand {
    manifest: PathBuf::from(DEFAULT_MANIFEST),
    output: Vec::new(),
    runtime: Vec::new(),
    source: Vec::new(),
    install_directory: Vec::new(),
    frontend: None,
    force: false,
    target: Vec::new(),
});

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct InitCommand {
    #[arg(long, default_value = DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    app_id: Option<String>,
    #[arg(long, default_value = "0.1.0")]
    version: String,
    #[arg(long, value_hint = ValueHint::DirPath)]
    source: Option<String>,
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    #[arg(long, value_enum)]
    frontend: Option<FrontendArg>,
    #[arg(long)]
    main: Option<String>,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    non_interactive: bool,
}

#[cfg(feature = "build")]
default_manifest!(InitCommand {
    manifest: PathBuf::from(DEFAULT_MANIFEST),
    name: None,
    app_id: None,
    version: "0.1.0".to_owned(),
    source: None,
    scope: None,
    frontend: None,
    main: None,
    force: false,
    non_interactive: false,
});

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct CheckCommand {
    #[arg(long, default_value = DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    /// Build source directory for each selected target, relative to the project.
    #[arg(long, value_hint = ValueHint::DirPath)]
    source: Vec<PathBuf>,
    /// Default install directory to resolve for each selected target.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    install_directory: Vec<PathBuf>,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    target: Vec<String>,
}

#[cfg(feature = "build")]
default_manifest!(CheckCommand {
    manifest: PathBuf::from(DEFAULT_MANIFEST),
    source: Vec::new(),
    install_directory: Vec::new(),
    target: Vec::new(),
});

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct PlanCommand {
    #[arg(long, default_value = DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    target: Vec<String>,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long = "enable")]
    enable: Vec<String>,
    #[arg(long = "disable")]
    disable: Vec<String>,
    #[arg(long = "install-directory", alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    install_directory: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[cfg(feature = "build")]
default_manifest!(PlanCommand {
    manifest: PathBuf::from(DEFAULT_MANIFEST),
    target: Vec::new(),
    scope: ScopeArg::User,
    state_root: None,
    enable: Vec::new(),
    disable: Vec::new(),
    install_directory: None,
    json: false,
});

#[cfg(feature = "build")]
#[derive(Debug, Args, Default)]
struct SchemaCommand {
    #[arg(long, value_hint = ValueHint::FilePath)]
    output: Option<PathBuf>,
}

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct FmtCommand {
    #[arg(long, default_value = DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long)]
    check: bool,
}

#[cfg(feature = "build")]
default_manifest!(FmtCommand {
    manifest: PathBuf::from(DEFAULT_MANIFEST),
    check: false,
});

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct CompletionsCommand {
    #[arg(value_enum)]
    shell: clap_complete::Shell,
}

static OUTPUT_FAILURE_EMITTED: AtomicBool = AtomicBool::new(false);
static FRONTEND_OVERRIDE: AtomicU8 = AtomicU8::new(0);

pub fn process_exit_code(error: &miette::Report) -> u8 {
    ProcessOutcome::from_message(&error.to_string())
        .code()
        .clamp(1, 255) as u8
}

pub fn run() -> miette::Result<()> {
    run_internal(None)
}

pub fn run_as(frontend: Frontend) -> miette::Result<()> {
    run_internal(Some(frontend))
}

fn run_internal(runtime_frontend: Option<Frontend>) -> miette::Result<()> {
    let cli = Cli::parse();
    let output = cli.output_format();
    FRONTEND_OVERRIDE.store(
        runtime_frontend.map_or(0, |frontend| match frontend {
            Frontend::Gui => 1,
            Frontend::Console => 2,
            Frontend::Headless => 3,
        }),
        Ordering::SeqCst,
    );
    let result = run_command(cli, runtime_frontend);
    FRONTEND_OVERRIDE.store(0, Ordering::SeqCst);
    if let Err(error) = &result {
        if !OUTPUT_FAILURE_EMITTED.swap(false, Ordering::SeqCst) {
            emit_failure(output, error);
        }
    } else {
        OUTPUT_FAILURE_EMITTED.store(false, Ordering::SeqCst);
    }
    result
}

fn run_command(cli: Cli, runtime_frontend: Option<Frontend>) -> miette::Result<()> {
    match cli.command {
        #[cfg(feature = "build")]
        Some(Commands::Build(args)) => run_build(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Init(args)) => run_init(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Check(args)) => run_check(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Doctor(args)) => doctor::run(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Plan(args)) => run_plan(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Schema(args)) => run_schema(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Fmt(args)) => run_fmt(args)?,
        #[cfg(feature = "build")]
        Some(Commands::Completions(args)) => run_completions(args)?,
        Some(Commands::Install(args)) => run_manifest_transition(LifecycleAction::Install, args)?,
        Some(Commands::Upgrade(args)) => run_manifest_transition(LifecycleAction::Upgrade, args)?,
        Some(Commands::Update(args)) => run_update(args)?,
        Some(Commands::Modify(args)) => run_manifest_transition(LifecycleAction::Modify, args)?,
        Some(Commands::Repair(args)) => run_manifest_transition(
            LifecycleAction::Repair {
                force_files: args.force_files,
            },
            args.install,
        )?,
        Some(Commands::Uninstall(args)) => run_uninstall(args)?,
        Some(Commands::UninstallRunner(args)) => {
            zup_windows::wait_for_process_exit(args.wait_pid)
                .map_err(|error| miette::miette!("wait for maintenance process: {error}"))?;
            let cleanup_path =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            #[cfg(feature = "gui")]
            let result = if effective_frontend() == Frontend::Gui && args.uninstall.ui {
                let executable = zup_windows::current_exe()
                    .map_err(|error| miette::miette!("executable: {error}"))?;
                let bundle = zup_windows::EmbeddedBundle::open(&executable)
                    .map_err(|error| miette::miette!("installer package: {error}"))?;
                run_graphical_frontend(executable, &bundle, true, true)
            } else {
                run_uninstall(args.uninstall)
            };
            #[cfg(not(feature = "gui"))]
            let result = if args.uninstall.ui {
                Err(miette::miette!(
                    "the GUI frontend is not available in this runtime"
                ))
            } else {
                run_uninstall(args.uninstall)
            };
            schedule_runner_cleanup(&cleanup_path);
            result?;
        }
        Some(Commands::Recover(args)) => run_recover(args)?,
        Some(Commands::Worker { bootstrap }) => run_worker_mode(&bootstrap)?,
        Some(Commands::FrontendInfo) => {
            println!("{}", runtime_frontend.unwrap_or_else(compiled_frontend))
        }
        Some(Commands::WorkerHelp) => {
            println!(
                "zup __worker <protocol>|<session>|<pipe>|<parent_pid>|<parent_sid>|<target>|<plan_hash>"
            );
        }
        None => {
            let executable =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            match zup_windows::EmbeddedBundle::open(&executable) {
                #[cfg(feature = "gui")]
                Ok(bundle) if effective_frontend() == Frontend::Gui => {
                    run_graphical_frontend(executable, &bundle, false, false)?
                }
                #[cfg(feature = "console")]
                Ok(bundle) if effective_frontend() == Frontend::Console => {
                    run_console_frontend(executable, &bundle, false, false)?
                }
                Ok(_bundle) => {
                    return Err(miette::miette!(
                        "this runtime requires an install, upgrade, or other command"
                    ));
                }
                Err(error) if error.is_missing_resource() => {
                    return Err(miette::miette!(
                        "installer package unavailable; run zup build or use a command with --manifest"
                    ));
                }
                Err(error) => return Err(miette::miette!("installer package: {error}")),
            }
        }
    }
    Ok(())
}

impl Cli {
    fn output_format(&self) -> OutputFormat {
        match &self.command {
            Some(Commands::Update(args)) => args.output.into(),
            #[cfg(feature = "build")]
            Some(Commands::Doctor(args)) => args.format.into(),
            Some(Commands::Install(args))
            | Some(Commands::Upgrade(args))
            | Some(Commands::Modify(args)) => args.output.into(),
            Some(Commands::Repair(args)) => args.install.output.into(),
            Some(Commands::Uninstall(args)) => args.output.into(),
            Some(Commands::UninstallRunner(args)) => args.uninstall.output.into(),
            Some(Commands::Recover(args)) => args.output.into(),
            _ => OutputFormat::Human,
        }
    }
}

fn emit_failure(output: OutputFormat, error: &miette::Report) {
    let outcome = ProcessOutcome::from_message(&error.to_string());
    match output {
        OutputFormat::Human => {}
        OutputFormat::Json => {
            let mut result = AutomationResult::new(outcome, "", "");
            result.message = Some(error.to_string());
            if let Ok(value) = result.to_json() {
                println!("{value}");
            }
        }
        OutputFormat::Jsonl => {
            let started = AutomationEvent::started("", "", "unknown");
            if let Ok(value) = serde_json::to_string(&started) {
                println!("{value}");
            }
            let event = AutomationEvent::Failed {
                outcome,
                code: outcome.code(),
                message: error.to_string(),
                diagnostic: Some(zup_presentation::DiagnosticPresentation::from_message(
                    &error.to_string(),
                    outcome == ProcessOutcome::RecoveryRequired,
                )),
            };
            if let Ok(value) = serde_json::to_string(&event) {
                println!("{value}");
            }
        }
    }
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

fn install_directory_template(path: &Path) -> miette::Result<zup_core::Template> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(miette::miette!(
            "install directory must not contain template variables: {value}"
        ));
    }
    zup_core::Template::parse(&value).map_err(|error| miette::miette!("install directory: {error}"))
}

fn persisted_install_directory(
    ledger: Option<&zup_exec::InstallLedger>,
) -> Option<zup_core::Template> {
    ledger
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| zup_core::Template::parse(&path.to_string()).ok())
}

fn choose_install_directory(
    explicit: Option<&Path>,
    ledger: Option<&zup_exec::InstallLedger>,
    allowed: bool,
) -> miette::Result<Option<zup_core::Template>> {
    if let Some(path) = explicit {
        if !allowed {
            return Err(miette::miette!(
                "this application does not allow choosing an install directory"
            ));
        }
        return install_directory_template(path).map(Some);
    }
    if !allowed {
        return Ok(None);
    }
    Ok(persisted_install_directory(ledger))
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

fn target_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        text.into_owned()
    }
}

/// The resolved install directory of a target, as one line.
///
/// A single scope's template is shown on its own; both are labeled when the
/// target installs to two scopes.
#[cfg(feature = "build")]
fn install_directory_text(install: &zup_core::Install) -> String {
    let user = install.directory.user.as_ref().map(ToString::to_string);
    let machine = install.directory.machine.as_ref().map(ToString::to_string);
    match (user, machine) {
        (Some(user), None) => user,
        (None, Some(machine)) => machine,
        (None, None) => "no directory template".to_owned(),
        (Some(user), Some(machine)) => format!("user={user} machine={machine}"),
    }
}

/// Create the directory an output will be written into, so a caller naming a
/// directory that does not exist yet gets the installer instead of an I/O error.
#[cfg(feature = "build")]
fn ensure_output_parent(output: &Path) -> miette::Result<()> {
    let Some(parent) = output.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent)
        .map_err(|error| miette::miette!("output directory {}: {error}", parent.display()))
}

/// A staging path beside an output, on the same volume so the move into place
/// cannot cross a filesystem boundary.
#[cfg(feature = "build")]
fn staging_output(output: &Path) -> miette::Result<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| miette::miette!("output `{}` has no file name", output.display()))?;
    let staging = output.with_file_name(format!(
        "{}.{}.staging",
        name.to_string_lossy(),
        std::process::id()
    ));
    if staging.exists() {
        std::fs::remove_file(&staging)
            .map_err(|error| miette::miette!("stale staging file: {error}"))?;
    }
    Ok(staging)
}

/// Put a finished artifact where the caller asked for it, replacing whatever
/// `--force` authorized replacing.
#[cfg(feature = "build")]
fn replace_output(staging: &Path, output: &Path) -> miette::Result<()> {
    if output.exists() {
        std::fs::remove_file(output)
            .map_err(|error| miette::miette!("replace `{}`: {error}", output.display()))?;
    }
    std::fs::rename(staging, output).map_err(|error| {
        miette::miette!(
            "move `{}` to `{}`: {error}",
            staging.display(),
            output.display()
        )
    })
}

#[cfg(feature = "build")]
fn run_build(args: BuildCommand) -> miette::Result<()> {
    let overrides = TargetOverrideArgs {
        source: args.source,
        install_directory: args.install_directory,
        frontend: args.frontend.map(Frontend::from),
    };
    let selected = select_project(&args.manifest, &args.target, &overrides, false)?;
    // The backend boundary reads no files, so an unsupported target is refused
    // before the source tree is walked and prerequisites are resolved.
    for config in &selected.selected_targets {
        build_inputs::check_backend_support(config)?;
    }
    let loaded = materialize_project(selected)?;
    let targets = &loaded.selected_targets;
    for config in targets {
        build_inputs::check_target_lowering(&loaded.build, config)?;
    }
    let inputs = build_inputs::resolve_build_inputs(
        build_inputs::InputMode::Enforce,
        build_inputs::Overwrite::from(args.force),
        &args.runtime,
        &args.output,
        &loaded.manifest_path,
        &loaded.manifest.app,
        targets,
    )?;
    let runtimes = inputs
        .runtimes
        .iter()
        .map(|slot| {
            slot.path
                .clone()
                .expect("enforced inputs have no unresolved runtime")
        })
        .collect::<Vec<_>>();
    let outputs = inputs
        .outputs
        .iter()
        .map(|slot| slot.path.clone())
        .collect::<Vec<_>>();

    let interactive = std::io::stdout().is_terminal();
    if interactive {
        println!("→ Validating manifest");
        println!("→ Materializing payload");
    }

    let mut checked_runtimes = Vec::with_capacity(targets.len());
    for ((config, target_plan), runtime) in targets.iter().zip(&loaded.build.targets).zip(&runtimes)
    {
        let runtime_target = zup_windows::read_pe_target(runtime)
            .map_err(|error| miette::miette!("runtime target: {error}"))?;
        if runtime_target != config.target {
            return Err(miette::miette!(
                "runtime target `{runtime_target}` does not match target `{}`",
                config.target
            ));
        }
        zup_windows::validate_pe_frontend(runtime, config.frontend)
            .map_err(|error| miette::miette!("runtime frontend: {error}"))?;
        build_inputs::validate_runtime_template(runtime, config.frontend)?;
        checked_runtimes.push((target_plan, config, runtime));
    }

    let mut prepared = Vec::with_capacity(targets.len());
    for (target_plan, config, runtime) in checked_runtimes {
        let plugin_artifacts = zup_plugin_build::compile_plugins(target_plan).map_err(|error| {
            miette::miette!("plugin compilation for `{}`: {error}", config.profile)
        })?;
        prepared.push((target_plan, config, runtime, plugin_artifacts));
    }

    if interactive {
        println!("→ Compiling plugins");
    }
    for ((target_plan, config, runtime, plugin_artifacts), output) in prepared.iter().zip(&outputs)
    {
        if interactive {
            println!("→ Compressing and embedding {}", config.profile);
        }
        // The backend never writes over an existing file, so `--force` writes
        // beside the destination and moves the finished artifact into place.
        ensure_output_parent(output)?;
        let staging = if args.force && output.exists() {
            Some(staging_output(output)?)
        } else {
            None
        };
        let written = match &staging {
            Some(staging) => staging.as_path(),
            None => output.as_path(),
        };
        let (size, _) = zup_windows::build_self_contained_executable(
            runtime,
            written,
            target_plan,
            plugin_artifacts,
        )
        .map_err(|error| miette::miette!("installer output: {error}"))?;
        if let Some(staging) = staging
            && let Err(error) = replace_output(&staging, output)
        {
            let _ = std::fs::remove_file(&staging);
            return Err(error);
        }
        let payload_bytes: u64 = target_plan.files.iter().map(|file| file.size).sum();
        let updates = target_plan
            .installer
            .updates
            .as_ref()
            .map(|updates| updates.channel.as_str())
            .unwrap_or("not configured");
        println!(
            "Built {} {} ({})",
            target_plan.installer.app.name, target_plan.installer.app.version, config.profile
        );
        println!("  Frontend    {}", config.frontend);
        println!("  Installer   {}", output.display());
        println!("  Size        {}", zup_presentation::format_bytes(size));
        println!("  Target      {}", config.target);
        println!(
            "  Payload     {} files · {}",
            target_plan.files.len(),
            zup_presentation::format_bytes(payload_bytes)
        );
        println!("  Plugins     {}", plugin_artifacts.len());
        println!("  Updates     {updates}");
    }
    if interactive {
        println!("\n✓ Ready to sign");
    }
    Ok(())
}

#[cfg(feature = "build")]
fn toml_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            character if character.is_control() => {
                use std::fmt::Write;
                let _ = write!(output, "\\u{:04X}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

#[cfg(feature = "build")]
fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "app".into()
    } else {
        output
    }
}

#[cfg(feature = "build")]
fn run_init(args: InitCommand) -> miette::Result<()> {
    let manifest_path = args.manifest.canonicalize().unwrap_or_else(|_| {
        if args.manifest.is_absolute() {
            args.manifest.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(&args.manifest)
        }
    });
    if manifest_path.exists() && !args.force {
        return Err(miette::miette!(
            "{} already exists; pass --force to replace it",
            manifest_path.display()
        ));
    }
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let directory_name = manifest_path
        .parent()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("app");
    let mut name = args.name;
    let mut app_id = args.app_id;
    let mut source = args.source;
    let mut scope = args.scope;
    let mut main = args.main;
    if interactive {
        if name.is_none() {
            name = Some(
                inquire::Text::new("Application name")
                    .with_default(directory_name)
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if app_id.is_none() {
            let default_id = format!("com.example.{}", slug(name.as_deref().unwrap_or("app")));
            app_id = Some(
                inquire::Text::new("Application ID")
                    .with_default(&default_id)
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if source.is_none() {
            source = Some(
                inquire::Text::new("Source directory")
                    .with_default("dist")
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if scope.is_none() {
            let value = inquire::Select::new("Install scope", vec!["user", "machine", "either"])
                .with_starting_cursor(0)
                .prompt()
                .map_err(|error| miette::miette!("prompt: {error}"))?;
            scope = Some(match value {
                "machine" => ScopeArg::Machine,
                "either" => ScopeArg::Either,
                _ => ScopeArg::User,
            });
        }
        if main.is_none() {
            main = Some(
                inquire::Text::new("Main executable")
                    .with_default("app.exe")
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
    }
    let name = name.ok_or_else(|| miette::miette!("--name is required in non-interactive mode"))?;
    let app_id =
        app_id.ok_or_else(|| miette::miette!("--app-id is required in non-interactive mode"))?;
    let source = source.unwrap_or_else(|| "dist".into());
    let scope = scope.unwrap_or(ScopeArg::User);
    let main = main.unwrap_or_else(|| "app.exe".into());
    semver::Version::parse(&args.version).map_err(|error| miette::miette!("version: {error}"))?;
    let scope_name = match scope {
        ScopeArg::User => "user",
        ScopeArg::Machine => "machine",
        ScopeArg::Either => "either",
    };
    let install_name = slug(&name);
    let frontend = args.frontend.map(Frontend::from).unwrap_or_default();
    let target = build_inputs::default_build_target();
    let mut document = format!(
        "#:schema https://zup.dev/schema/zup.toml.json\n\nschema = {}\nfrontend = {}\n\n[app]\nid = {}\nname = {}\nversion = {}\nmain = {}\n\n[build]\n\n[build.targets.default]\ntarget = {}\nsource = {{ directory = {} }}\n\n[install]\nscope = {}\nallow_directory_override = true\n\n[install.directory]\n",
        zup_manifest::SCHEMA_VERSION,
        toml_string(frontend.as_str()),
        toml_string(&app_id),
        toml_string(&name),
        toml_string(&args.version),
        toml_string(&main),
        toml_string(&target),
        toml_string(&source),
        toml_string(scope_name),
    );
    if scope != ScopeArg::Machine {
        document.push_str(&format!(
            "user = \"${{location.user_data}}/{install_name}\"\n"
        ));
    }
    if scope != ScopeArg::User {
        document.push_str(&format!(
            "machine = \"${{location.programs}}/{install_name}\"\n"
        ));
    }
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| miette::miette!("project directory: {error}"))?;
        let source_path = Path::new(&source);
        let source_path = if source_path.is_absolute() {
            source_path.to_path_buf()
        } else {
            parent.join(source_path)
        };
        std::fs::create_dir_all(source_path)
            .map_err(|error| miette::miette!("source directory: {error}"))?;
    }
    std::fs::write(&manifest_path, document)
        .map_err(|error| miette::miette!("write manifest: {error}"))?;
    println!("Created {}", manifest_path.display());
    println!("Next: edit zup.toml, then run zup check");
    Ok(())
}

#[cfg(feature = "build")]
#[derive(Debug)]
struct LoadedProject {
    manifest_path: PathBuf,
    manifest: zup_manifest::Manifest,
    selected_targets: Vec<zup_manifest::ResolvedTargetConfig>,
    build: zup_build::BuildPlan,
}

/// A read manifest and its selected profiles, before anything is materialized.
///
/// Selection touches no source tree, so a caller can reject a target this host
/// cannot build before paying for materialization.
#[cfg(feature = "build")]
#[derive(Debug)]
struct SelectedProject {
    manifest_path: PathBuf,
    manifest_name: String,
    source: String,
    manifest: zup_manifest::Manifest,
    /// The caller's per-profile overrides, in the form `compile` revalidates.
    overrides: zup_manifest::TargetOverrideSet,
    selected_targets: Vec<zup_manifest::ResolvedTargetConfig>,
}

/// The caller-supplied per-target overrides of an authoring command.
///
/// Every repeatable flag here is aligned against the selected target count, so a
/// single value against several targets is an error rather than a broadcast.
#[cfg(feature = "build")]
#[derive(Debug, Clone, Default)]
struct TargetOverrideArgs {
    source: Vec<PathBuf>,
    install_directory: Vec<PathBuf>,
    frontend: Option<Frontend>,
}

#[cfg(feature = "build")]
impl TargetOverrideArgs {
    /// The per-profile overrides, aligned with the selected profiles in order.
    fn resolve(
        &self,
        selected: &[zup_manifest::ResolvedTargetConfig],
    ) -> miette::Result<zup_manifest::TargetOverrideSet> {
        let targets = selected.len();
        let sources = build_inputs::align_per_target("sources", "--source", &self.source, targets)?;
        let directories = build_inputs::align_per_target(
            "install directories",
            "--install-directory",
            &self.install_directory,
            targets,
        )?;
        let mut overrides = zup_manifest::TargetOverrideSet::default();
        for (index, config) in selected.iter().enumerate() {
            let source = sources
                .and_then(|sources| sources.get(index))
                .map(|path| zup_core::Source::new(path.clone()))
                .transpose()
                .map_err(|error| miette::miette!("--source: {error}"))?;
            let install_directory = directories
                .and_then(|directories| directories.get(index))
                .map(|path| install_directory_template(path))
                .transpose()?;
            overrides.apply(
                config.profile.clone(),
                TargetOverrides {
                    source,
                    install_directory,
                    frontend: self.frontend,
                },
            );
        }
        Ok(overrides)
    }
}

/// Read a manifest and resolve its selected targets without materializing them.
#[cfg(feature = "build")]
fn select_project(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
    single: bool,
) -> miette::Result<SelectedProject> {
    let manifest_path = path
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source = std::fs::read_to_string(&manifest_path)
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let manifest_name = target_path_text(&manifest_path);
    let manifest =
        zup_manifest::parse_named(&source, &manifest_name).map_err(miette::Report::new)?;
    let effective = if single && selectors.is_empty() {
        if manifest.build.targets.len() != 1 {
            return Err(miette::miette!(
                "this command requires exactly one target; pass --target when the manifest declares multiple profiles"
            ));
        }
        vec![
            manifest
                .build
                .targets
                .keys()
                .next()
                .expect("manifest has one target")
                .to_string(),
        ]
    } else {
        selectors.to_vec()
    };
    let selector_refs = effective.iter().map(String::as_str).collect::<Vec<_>>();
    // The un-overridden selection names the profiles and their count, which is
    // what the repeatable flags align against.
    let selection =
        zup_manifest::select_targets(&manifest, &selector_refs, &TargetOverrides::default())
            .map_err(|error| {
                miette::Report::new(error.with_source_named(&source, &manifest_name))
            })?;
    let overrides = args.resolve(&selection)?;
    let selected_targets = zup_manifest::select_targets_with(&manifest, &selector_refs, &overrides)
        .map_err(|error| miette::Report::new(error.with_source_named(&source, &manifest_name)))?;
    if single && selected_targets.len() != 1 {
        return Err(miette::miette!(
            "this command accepts exactly one target; pass one --target"
        ));
    }
    Ok(SelectedProject {
        manifest_path,
        manifest_name,
        source,
        manifest,
        overrides,
        selected_targets,
    })
}

/// Compile and materialize the selected targets of a project.
#[cfg(feature = "build")]
fn materialize_project(selected: SelectedProject) -> miette::Result<LoadedProject> {
    let SelectedProject {
        manifest_path,
        manifest_name,
        source,
        manifest,
        overrides,
        selected_targets,
        ..
    } = selected;
    let compiled = selected_targets
        .iter()
        .map(|config| {
            zup_manifest::compile(&manifest, config, overrides.get(&config.profile))
                .map(|installer| (config.clone(), installer))
                .map_err(|error| {
                    miette::Report::new(error.with_source_named(&source, &manifest_name))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let build = zup_build::materialize_with_policy(
        &manifest_path,
        &manifest,
        compiled,
        &zup_windows::WindowsSourceFilePolicy,
    )
    .map_err(miette::Report::new)?;
    Ok(LoadedProject {
        manifest_path,
        manifest,
        selected_targets,
        build,
    })
}

#[cfg(feature = "build")]
fn load_single_project(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
) -> miette::Result<LoadedProject> {
    materialize_project(select_project(path, selectors, args, true)?)
}

fn prepare_bootstrap(
    build: &zup_plan::TargetBuildPlan,
    install: &zup_plan::InstallPlan,
    state_root: &Path,
    scope: SelectedScope,
    bundle: Option<&zup_windows::EmbeddedBundle>,
) -> miette::Result<Option<zup_runtime::BootstrapRequest>> {
    if install.prerequisites.is_empty() {
        return Ok(None);
    }
    let operations = install
        .prerequisites
        .iter()
        .map(|prerequisite| BootstrapOperation {
            id: prerequisite.id.clone(),
            name: prerequisite.name.to_string(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        })
        .collect::<Vec<_>>();
    let plan = BootstrapPlan::new(
        BootstrapKey {
            app_id: install.app.id.clone(),
            app_version: install.app.version.clone(),
            scope,
            target: install.target.clone(),
        },
        operations,
    )
    .map_err(|error| miette::miette!("prerequisite plan: {error}"))?;
    let id = BootstrapId::for_plan(&plan);
    let mut state = BootstrapState::new(&plan);
    state.id = id;
    let satisfier = zup_windows::WindowsPrerequisiteDetector;
    zup_bootstrap::assess(&plan, &satisfier, &mut state)
        .map_err(|error| miette::miette!("prerequisite detection: {error}"))?;
    let quarantine_root = if scope == SelectedScope::Machine {
        default_state_root(SelectedScope::User)?
            .join("bootstrap-acquisition")
            .join(id.as_uuid().to_string())
    } else {
        state_root
            .join("bootstrap")
            .join("quarantine")
            .join(id.as_uuid().to_string())
    };
    let quarantine = Quarantine::with_file_system(
        &quarantine_root,
        zup_windows::windows_bootstrap_file_system(),
    )
    .map_err(|error| miette::miette!("prerequisite quarantine: {error}"))?;
    if state.remaining.is_empty() {
        let _ = zup_bootstrap::FilesystemBootstrapStateStore::with_file_system(
            state_root,
            zup_windows::windows_bootstrap_file_system(),
        )
        .remove(id);
        let bound = BoundBootstrapPlan::with_id(id, plan, BTreeMap::new())
            .map_err(|error| miette::miette!("bind prerequisite plan: {error}"))?;
        return Ok(Some(zup_runtime::BootstrapRequest {
            plan: bound,
            state_root: state_root.to_path_buf(),
            quarantine_root,
        }));
    }
    let mut artifacts = BTreeMap::new();
    for operation in &plan.operations {
        if !state.remaining.contains(&operation.id) {
            continue;
        }
        let reservation = quarantine
            .reserve(
                &operation.id,
                operation.package.filename(),
                operation.package.size(),
            )
            .map_err(|error| miette::miette!("prerequisite reservation: {error}"))?;
        quarantine
            .remove_partial(&reservation)
            .map_err(|error| miette::miette!("clear prerequisite staging: {error}"))?;
        let artifact = match &operation.package {
            zup_core::PrerequisitePackage::Embedded { .. } => {
                if let Some(bundle) = bundle {
                    let bytes = bundle
                        .prerequisite_bytes(&operation.id)
                        .map_err(|error| miette::miette!("embedded prerequisite: {error}"))?;
                    quarantine
                        .stage_bytes(&reservation, &bytes, operation.package.digest())
                        .map_err(|error| miette::miette!("stage prerequisite: {error}"))?
                } else {
                    let resolved = build
                        .prerequisites
                        .iter()
                        .find(|item| item.id == operation.id)
                        .ok_or_else(|| {
                            miette::miette!("embedded prerequisite source is missing")
                        })?;
                    let file = std::fs::File::open(&resolved.source)
                        .map_err(|error| miette::miette!("read prerequisite: {error}"))?;
                    quarantine
                        .stage_reader(&reservation, file, operation.package.digest())
                        .map_err(|error| miette::miette!("stage prerequisite: {error}"))?
                }
            }
            zup_core::PrerequisitePackage::Remote {
                url, sha256, size, ..
            } => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| miette::miette!("download runtime: {error}"))?;
                runtime
                    .block_on(zup_update::download_pinned(
                        url,
                        *sha256,
                        *size,
                        &reservation.partial_path,
                        zup_core::MAX_PREREQUISITE_PACKAGE_BYTES,
                    ))
                    .map_err(|error| miette::miette!("download prerequisite: {error}"))?;
                quarantine
                    .publish(&reservation, *sha256)
                    .map_err(|error| miette::miette!("verify prerequisite: {error}"))?
            }
        };
        artifacts.insert(operation.id.clone(), artifact);
    }
    let bound = BoundBootstrapPlan::with_id(id, plan, artifacts)
        .map_err(|error| miette::miette!("bind prerequisite plan: {error}"))?;
    Ok(Some(zup_runtime::BootstrapRequest {
        plan: bound,
        state_root: state_root.to_path_buf(),
        quarantine_root,
    }))
}

#[cfg(feature = "build")]
fn run_check(args: CheckCommand) -> miette::Result<()> {
    let overrides = TargetOverrideArgs {
        source: args.source,
        install_directory: args.install_directory,
        frontend: None,
    };
    let selected = select_project(&args.manifest, &args.target, &overrides, false)?;
    // Same boundary as `zup build`, at the same point: before materialization.
    for config in &selected.selected_targets {
        build_inputs::check_backend_support(config)?;
    }
    let loaded = materialize_project(selected)?;
    for (config, target_plan) in loaded.selected_targets.iter().zip(&loaded.build.targets) {
        build_inputs::check_target_lowering(&loaded.build, config)?;
        if !target_plan.installer.plugins.is_empty() {
            zup_plugin_build::compile_plugins(target_plan).map_err(|error| {
                miette::miette!("plugin check for `{}`: {error}", config.profile)
            })?;
        }
        println!(
            "✓ {} is valid ({})",
            target_plan.installer.app.name, config.profile
        );
        println!("  Target      {}", config.target);
        println!("  Source      {}", config.source.directory.display());
        println!(
            "  Install     {} · {}",
            config.install.scope,
            install_directory_text(&config.install)
        );
        println!("  Components  {}", target_plan.installer.components.len());
        println!("  Files       {}", target_plan.files.len());
        println!("  Plugins     {}", target_plan.installer.plugins.len());
    }
    Ok(())
}

#[cfg(feature = "build")]
fn run_plan(args: PlanCommand) -> miette::Result<()> {
    let loaded = load_single_project(&args.manifest, &args.target, &TargetOverrideArgs::default())?;
    let config = loaded
        .selected_targets
        .first()
        .expect("single target selection");
    let installer = &loaded.build.targets[0].installer;
    let scope = SelectedScope::from(args.scope);
    let state_root = choose_state_root(args.state_root, scope)?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let mut request = zup_plan::PlanRequest::new(config.target.clone(), scope);
    request.install_directory = choose_install_directory(
        args.install_directory.as_deref(),
        prior.as_ref(),
        installer.install.allow_directory_override,
    )?;
    for raw in args.enable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.enable.insert(id);
    }
    for raw in args.disable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.disable.insert(id);
    }
    let install = zup_plan::plan(&loaded.build, &request).map_err(miette::Report::new)?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let transaction = zup_windows::plan_target_lifecycle(
        LifecycleAction::Install,
        &installer.app.id,
        scope,
        Some(&target),
        &state_root,
    )
    .map_err(|error| miette::miette!("transaction plan: {error}"))?;
    let mut preview = zup_presentation::PlanPreview::from_transaction_plan(&transaction, scope)
        .with_prerequisites(&install);
    preview.application = installer.app.name.to_string();
    preview.version = installer.app.version.to_string();
    preview.install_directory = target.install_directory.to_string();
    if args.json {
        let value = serde_json::json!({
            "preview": preview,
            "transaction": transaction,
            "target": target,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
    } else {
        println!("{}", preview.human());
    }
    Ok(())
}

#[cfg(feature = "build")]
fn run_schema(args: SchemaCommand) -> miette::Result<()> {
    let json = zup_manifest::schema_json().map_err(|error| miette::miette!("schema: {error}"))?;
    if let Some(path) = args.output {
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|error| miette::miette!("write schema: {error}"))?;
    } else {
        println!("{json}");
    }
    Ok(())
}

#[cfg(feature = "build")]
fn run_fmt(args: FmtCommand) -> miette::Result<()> {
    let path = args
        .manifest
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source =
        std::fs::read_to_string(&path).map_err(|error| miette::miette!("manifest: {error}"))?;
    zup_manifest::parse_named(&source, &path.display().to_string()).map_err(miette::Report::new)?;
    let document = source
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| miette::miette!("format manifest: {error}"))?;
    let mut formatted = document.to_string();
    if !formatted.ends_with('\n') {
        formatted.push('\n');
    }
    if args.check {
        if formatted != source {
            return Err(miette::miette!("{} is not formatted", path.display()));
        }
        println!("{} is formatted", path.display());
    } else if formatted != source {
        std::fs::write(&path, formatted)
            .map_err(|error| miette::miette!("write manifest: {error}"))?;
        println!("Formatted {}", path.display());
    } else {
        println!("{} is already formatted", path.display());
    }
    Ok(())
}

#[cfg(feature = "build")]
fn run_completions(args: CompletionsCommand) -> miette::Result<()> {
    let mut command = Cli::command();
    clap_complete::generate(args.shell, &mut command, "zup", &mut std::io::stdout());
    Ok(())
}

fn embedded_target_plan(
    bundle: &zup_windows::EmbeddedBundle,
) -> miette::Result<zup_plan::TargetBuildPlan> {
    let build = bundle
        .build_plan()
        .map_err(|error| miette::miette!("installer plan: {error}"))?;
    let mut targets = build.targets;
    if targets.len() != 1 {
        return Err(miette::miette!(
            "embedded installer package must contain exactly one target"
        ));
    }
    Ok(targets.remove(0))
}

struct EmbeddedPreparationMode<'a> {
    cancellation: &'a dyn zup_plan::CancellationQuery,
    acquire_prerequisites: bool,
}

struct EmbeddedTransitionOptions {
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
    output: OutputFormat,
    policy: ExecutionPolicy,
}

#[derive(Clone)]
struct PreparedRuntime {
    request: RuntimeRequest,
    backend: std::sync::Arc<zup_windows::WindowsRuntimeBackend>,
}

impl PreparedRuntime {
    #[cfg(feature = "console")]
    fn with_overlay_policy(&self, policy: zup_windows::OverlayPolicy) -> Self {
        let backend = self.backend.as_ref().clone().with_overlay_policy(policy);
        Self {
            request: self.request.clone(),
            backend: std::sync::Arc::new(backend),
        }
    }
}

#[cfg(any(feature = "gui", all(test, feature = "build")))]
struct RuntimeCancellationQuery<'a>(&'a zup_runtime::CancellationHandle);

#[cfg(any(feature = "gui", all(test, feature = "build")))]
impl zup_plan::CancellationQuery for RuntimeCancellationQuery<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

fn run_embedded_transition_with_output(
    action: LifecycleAction,
    scope: SelectedScope,
    options: EmbeddedTransitionOptions,
) -> miette::Result<()> {
    let EmbeddedTransitionOptions {
        state,
        enable,
        disable,
        install_directory,
        output,
        policy,
    } = options;
    let request =
        prepare_embedded_transition(action, scope, state, enable, disable, install_directory)?;
    if output == OutputFormat::Human {
        #[cfg(feature = "console")]
        if policy == ExecutionPolicy::Interactive
            && effective_frontend() == Frontend::Console
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
        {
            return execute_console(request, action);
        }
        execute_with_policy(request, policy)
    } else {
        execute_frontend(request, output, action)
    }
}

fn prepare_embedded_transition(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
) -> miette::Result<PreparedRuntime> {
    prepare_embedded_transition_with_cancellation(
        action,
        scope,
        state,
        enable,
        disable,
        install_directory,
        EmbeddedPreparationMode {
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: true,
        },
    )
}

fn prepare_embedded_transition_with_cancellation(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
    mode: EmbeddedPreparationMode<'_>,
) -> miette::Result<PreparedRuntime> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(&executable)
        .map_err(|e| miette::miette!("installer package: {e}"))?;
    let build = embedded_target_plan(&bundle)?;
    let app_id = build.installer.app.id.clone();
    let plugin_target = build.installer.target.clone();
    let state_root = choose_state_root(state, scope)?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|e| miette::miette!("ledger: {e}"))?;
    prepare_embedded_request(
        EmbeddedPreparation {
            action,
            scope,
            build: &build,
            prior,
            state_root,
            payload_root: executable,
            enable,
            disable,
            install_directory,
        },
        Some(&bundle.clone()),
        mode.acquire_prerequisites,
        mode.cancellation,
        || {
            zup_plugin_runtime::WasmtimePluginExecutor::load(
                bundle.package().clone(),
                &plugin_target,
            )
            .map_err(|error| miette::miette!("plugin runtime: {error}"))
        },
    )
}

struct EmbeddedPreparation<'a> {
    action: LifecycleAction,
    scope: SelectedScope,
    build: &'a zup_plan::TargetBuildPlan,
    prior: Option<zup_exec::InstallLedger>,
    state_root: PathBuf,
    payload_root: PathBuf,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
}

fn prepare_embedded_request<E>(
    preparation: EmbeddedPreparation<'_>,
    embedded_bundle: Option<&zup_windows::EmbeddedBundle>,
    acquire_prerequisites: bool,
    cancellation: &dyn zup_plan::CancellationQuery,
    load_executor: impl FnOnce() -> Result<E, miette::Report>,
) -> miette::Result<PreparedRuntime>
where
    E: zup_plan::PluginExecutor,
{
    let EmbeddedPreparation {
        action,
        scope,
        build,
        prior,
        state_root,
        payload_root,
        enable,
        disable,
        install_directory,
    } = preparation;
    let app_id = build.installer.app.id.clone();
    if matches!(action, LifecycleAction::Repair { .. }) {
        if !enable.is_empty() || !disable.is_empty() {
            return Err(miette::miette!(
                "repair uses the committed component selection"
            ));
        }
        if install_directory.is_some() {
            return Err(miette::miette!(
                "repair uses the committed install location"
            ));
        }
    }
    let selected_install_directory = if action == LifecycleAction::Uninstall {
        None
    } else if matches!(action, LifecycleAction::Repair { .. }) {
        build
            .installer
            .install
            .allow_directory_override
            .then(|| persisted_install_directory(prior.as_ref()))
            .flatten()
    } else {
        choose_install_directory(
            install_directory.as_deref(),
            prior.as_ref(),
            build.installer.install.allow_directory_override,
        )?
    };
    if action == LifecycleAction::Uninstall {
        let ledger = prior.ok_or_else(|| miette::miette!("installation not found"))?;
        if ledger.target != build.installer.target {
            return Err(miette::miette!(
                "ledger target `{}` does not match embedded package target `{}`",
                ledger.target,
                build.installer.target
            ));
        }
        let execution = zup_windows::plan_target_lifecycle_with_frontend(
            action,
            &app_id,
            scope,
            None,
            &state_root,
            build.installer.frontend,
        )
        .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
        let request = RuntimeRequest {
            target: build.installer.target.clone(),
            app_id,
            app_version: ledger.version,
            scope,
            transaction_plan: execution,
            work_root: state_root.join("work"),
            state_root,
            recovery_id: None,
            bootstrap: None,
        };
        let backend = zup_windows::WindowsRuntimeBackend::from_path(payload_root, None)
            .map_err(|error| miette::miette!("payload source: {error}"))?;
        return Ok(PreparedRuntime {
            request,
            backend: std::sync::Arc::new(backend),
        });
    }

    let mut request = zup_plan::PlanRequest::new(build.installer.target.clone(), scope);
    request.install_directory = selected_install_directory;
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

    let mut executor = load_executor()?;
    let planning_build = zup_plan::BuildPlan {
        targets: vec![build.clone()],
    };
    let planned =
        zup_plan::plan_with_plugins(&planning_build, &request, &mut executor, cancellation)
            .map_err(|error| miette::miette!("plan: {error}"))?;
    let install = &planned.plan;
    let mut target =
        zup_windows::resolve_target(install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|e| miette::miette!("target: {e}"))?;
    let (size, sha256) = hash_reader(
        std::fs::File::open(&payload_root)
            .map_err(|error| miette::miette!("installer executable: {error}"))?,
    )
    .map_err(|error| miette::miette!("installer executable: {error}"))?;
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let destination = state_root
        .join("maintenance")
        .join(app_id.as_str())
        .join(scope_name)
        .join(target.app.version.to_string())
        .join("Setup.exe");
    let destination =
        zup_platform::TargetPath::new(target.target.clone(), target_path_text(&destination))
            .map_err(|error| miette::miette!("maintenance destination: {error}"))?;
    target.files.push(zup_platform::TargetFile {
        key: ResourceKey::Maintenance {
            app_id: app_id.to_string(),
            version: target.app.version.to_string(),
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new("__zup_maintenance__.exe").unwrap(),
        destination,
        size,
        sha256,
        // The maintenance executable lives in the scope's own state root, so
        // it needs that scope's authority and no more.
        privilege: scope.authorization(),
    });
    target.summary.file_count += 1;
    target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    target.summary.resource_count += 1;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        action,
        &app_id,
        scope,
        Some(&target),
        &state_root,
        build.installer.frontend,
    )
    .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
    let bootstrap = if acquire_prerequisites {
        prepare_bootstrap(build, install, &state_root, scope, embedded_bundle)?
    } else {
        None
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_path_with_generated_files(
        payload_root.clone(),
        &state_root,
        scope,
        install,
        &planned.generated_files,
        &execution,
    )
    .map_err(|error| miette::miette!("plugin payload overlay: {error}"))?;
    let work_root = state_root.join("work");
    let request = RuntimeRequest {
        target: target.target.clone(),
        app_id,
        app_version: target.app.version.clone(),
        scope,
        transaction_plan: execution,
        state_root,
        work_root,
        recovery_id: None,
        bootstrap,
    };
    Ok(PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
    })
}

#[cfg(feature = "console")]
struct ZupTheme;

#[cfg(feature = "console")]
impl cliclack::Theme for ZupTheme {
    fn bar_color(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Active => console::Style::new().cyan(),
            cliclack::ThemeState::Cancel | cliclack::ThemeState::Error(_) => {
                console::Style::new().red()
            }
            _ => console::Style::new().bright().black(),
        }
    }

    fn state_symbol_color(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Submit => console::Style::new().green(),
            _ => self.bar_color(state),
        }
    }
}

#[cfg(feature = "console")]
fn console_interactive(args: &ManifestCommand) -> bool {
    effective_frontend() == Frontend::Console
        && args.output == OutputArg::Human
        && !args.non_interactive
        && !args.yes
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

#[cfg(feature = "console")]
fn console_installation(
    installer: &zup_core::Installer,
    state_root: Option<&Path>,
    requested_scope: ScopeArg,
) -> miette::Result<Option<(SelectedScope, zup_exec::InstallLedger)>> {
    let scopes = match installer.install.scope {
        zup_core::InstallScope::User => vec![SelectedScope::User],
        zup_core::InstallScope::Machine => vec![SelectedScope::Machine],
        zup_core::InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };
    let mut found = Vec::new();
    for scope in scopes {
        if matches!(requested_scope, ScopeArg::User) && scope != SelectedScope::User
            || matches!(requested_scope, ScopeArg::Machine) && scope != SelectedScope::Machine
        {
            continue;
        }
        let state = choose_state_root(state_root.map(Path::to_path_buf), scope)?;
        let ledger = zup_windows::InstallLedgerStore::new(&state)
            .load(&installer.app.id, scope)
            .map_err(|error| miette::miette!("ledger: {error}"))?;
        if let Some(ledger) = ledger {
            found.push((scope, ledger));
        }
    }
    if found.len() > 1 {
        return Err(miette::miette!(
            "installation exists in both user and machine scopes; choose --scope"
        ));
    }
    Ok(found.pop())
}

#[cfg(feature = "console")]
fn console_scope(
    installer: &zup_core::Installer,
    requested: ScopeArg,
    installed: Option<SelectedScope>,
) -> miette::Result<SelectedScope> {
    if installer.install.scope == zup_core::InstallScope::Machine {
        return Ok(SelectedScope::Machine);
    }
    if installer.install.scope == zup_core::InstallScope::User {
        return Ok(SelectedScope::User);
    }
    if let Some(scope) = installed {
        return Ok(scope);
    }
    let initial = match requested {
        ScopeArg::Machine => SelectedScope::Machine,
        ScopeArg::User | ScopeArg::Either => SelectedScope::User,
    };
    let selected = cliclack::select("Install for")
        .item(SelectedScope::User, "Current user", "")
        .item(SelectedScope::Machine, "All users", "")
        .initial_value(initial)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?;
    Ok(selected)
}

#[cfg(feature = "console")]
fn console_components(
    installer: &zup_core::Installer,
    requested_enable: &[String],
    requested_disable: &[String],
    installed: Option<&zup_exec::InstallLedger>,
) -> miette::Result<Vec<ComponentId>> {
    if installer.components.is_empty() {
        return Ok(Vec::new());
    }
    let mut selected = installer
        .components
        .iter()
        .filter(|component| {
            component.required
                || component.default
                || installed
                    .is_some_and(|ledger| ledger.selected_components.contains(&component.id))
                || requested_enable
                    .iter()
                    .any(|value| value == component.id.as_str())
        })
        .map(|component| component.id.clone())
        .collect::<Vec<_>>();
    for value in requested_disable {
        if let Ok(id) = ComponentId::new(value) {
            selected.retain(|component| component != &id);
        }
    }
    if installer
        .components
        .iter()
        .all(|component| component.required)
        && selected.len() == installer.components.len()
    {
        return Ok(selected);
    }
    let items = installer
        .components
        .iter()
        .map(|component| {
            (
                component.id.clone(),
                component.name.to_string(),
                component.description.clone().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    let mut prompt = cliclack::multiselect("Components").required(false);
    for (id, name, description) in &items {
        prompt = prompt.item(id.clone(), name, description);
    }
    let mut chosen = prompt
        .initial_values(selected)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?;
    for component in &installer.components {
        if component.required && !chosen.contains(&component.id) {
            chosen.push(component.id.clone());
        }
    }
    Ok(chosen)
}

#[cfg(feature = "console")]
fn console_install_directory(
    installer: &zup_core::Installer,
    requested: Option<&Path>,
    installed: Option<&zup_exec::InstallLedger>,
) -> miette::Result<Option<PathBuf>> {
    if !installer.install.allow_directory_override {
        return Ok(requested.map(Path::to_path_buf));
    }
    if let Some(path) = requested {
        return Ok(Some(path.to_path_buf()));
    }
    let default = installed
        .and_then(|ledger| ledger.install_directory.as_ref())
        .map(ToString::to_string)
        .unwrap_or_default();
    let value: String = cliclack::input("Install location")
        .default_input(&default)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?;
    Ok((!value.trim().is_empty()).then(|| PathBuf::from(value)))
}

#[cfg(feature = "console")]
fn run_console_transition(
    action: LifecycleAction,
    mut args: ManifestCommand,
) -> miette::Result<()> {
    cliclack::set_theme(ZupTheme);
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(&executable)
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    let build = embedded_target_plan(&bundle)?;
    let installer = &build.installer;
    let installed = console_installation(installer, args.state_root.as_deref(), args.scope)?;
    let action = resolve_interactive_action(
        action,
        installed.as_ref().map(|(_, ledger)| &ledger.version),
        &installer.app.version,
    )?;
    let scope = console_scope(
        installer,
        args.scope,
        installed.as_ref().map(|(scope, _)| *scope),
    )?;
    let repair = matches!(action, LifecycleAction::Repair { .. });
    if repair && (!args.enable.is_empty() || !args.disable.is_empty()) {
        return Err(miette::miette!(
            "repair uses the committed component selection"
        ));
    }
    let components = if repair {
        Vec::new()
    } else {
        console_components(
            installer,
            &args.enable,
            &args.disable,
            installed.as_ref().map(|(_, ledger)| ledger),
        )?
    };
    let install_directory = if repair {
        if args.install_directory.is_some() {
            return Err(miette::miette!(
                "repair uses the committed install location"
            ));
        }
        None
    } else {
        console_install_directory(
            installer,
            args.install_directory.as_deref(),
            installed.as_ref().map(|(_, ledger)| ledger),
        )?
    };
    let location = install_directory
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the configured install directory".into());
    if !cliclack::confirm(format!("{} to {location}?", lifecycle_action_name(action)))
        .initial_value(true)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?
    {
        let _ = cliclack::outro_cancel("Cancelled");
        return Err(miette::miette!("cancelled"));
    }
    let _ = cliclack::outro("Ready to install");
    args.scope = match scope {
        SelectedScope::User => ScopeArg::User,
        SelectedScope::Machine => ScopeArg::Machine,
    };
    if repair {
        args.enable = Vec::new();
        args.disable = Vec::new();
    } else {
        args.enable = components.iter().map(ToString::to_string).collect();
        args.disable = installer
            .components
            .iter()
            .filter(|component| !components.contains(&component.id))
            .map(|component| component.id.to_string())
            .collect();
    }
    args.install_directory = install_directory;
    args.non_interactive = true;
    args.yes = true;
    run_manifest_transition_with_policy(action, args, ExecutionPolicy::Interactive)
}

#[cfg(feature = "console")]
fn run_console_frontend(
    executable: PathBuf,
    bundle: &zup_windows::EmbeddedBundle,
    _force_maintenance: bool,
    auto_uninstall: bool,
) -> miette::Result<()> {
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        return Err(miette::miette!(
            "the console frontend requires an interactive terminal; use a lifecycle command with --non-interactive"
        ));
    }
    cliclack::set_theme(ZupTheme);
    let build = embedded_target_plan(bundle)?;
    let installer = &build.installer;
    let app_id = &installer.app.id;
    let installed_scope = console_installation(installer, None, ScopeArg::Either)?;
    let action = if auto_uninstall {
        LifecycleAction::Uninstall
    } else {
        resolve_interactive_action(
            LifecycleAction::Install,
            installed_scope.as_ref().map(|(_, ledger)| &ledger.version),
            &installer.app.version,
        )?
    };
    let scope = console_scope(
        installer,
        ScopeArg::User,
        installed_scope.as_ref().map(|(scope, _)| *scope),
    )?;
    let components = console_components(
        installer,
        &[],
        &[],
        installed_scope.as_ref().map(|(_, ledger)| ledger),
    )?;
    let install_directory = console_install_directory(
        installer,
        None,
        installed_scope.as_ref().map(|(_, ledger)| ledger),
    )?;
    let location = install_directory
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the configured install directory".into());
    if !cliclack::confirm(format!("{} to {location}?", lifecycle_action_name(action)))
        .initial_value(true)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?
    {
        let _ = cliclack::outro_cancel("Cancelled");
        return Err(miette::miette!("cancelled"));
    }
    let _ = cliclack::outro("Ready to install");
    if auto_uninstall {
        return run_uninstall(UninstallCommand {
            ui: false,
            app_id: Some(app_id.to_string()),
            scope: Some(match scope {
                SelectedScope::User => ScopeArg::User,
                SelectedScope::Machine => ScopeArg::Machine,
            }),
            state_root: None,
            work_root: None,
            output: OutputArg::Human,
            non_interactive: false,
            yes: true,
        });
    }
    let _ = executable;
    run_embedded_transition_with_output(
        action,
        scope,
        EmbeddedTransitionOptions {
            state: None,
            enable: components.iter().map(ToString::to_string).collect(),
            disable: installer
                .components
                .iter()
                .filter(|component| !components.contains(&component.id))
                .map(|component| component.id.to_string())
                .collect(),
            install_directory,
            output: OutputFormat::Human,
            policy: ExecutionPolicy::Interactive,
        },
    )
}

fn run_manifest_transition(action: LifecycleAction, args: ManifestCommand) -> miette::Result<()> {
    let mut args = args;
    for component in args.component.drain(..) {
        args.enable.push(component);
    }
    #[cfg(feature = "gui")]
    if effective_frontend() == Frontend::Gui && args.ui {
        let executable =
            zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
        let bundle = zup_windows::EmbeddedBundle::open(&executable)
            .map_err(|error| miette::miette!("installer package: {error}"))?;
        return run_graphical_frontend(executable, &bundle, true, false);
    }
    #[cfg(feature = "gui")]
    if args.ui {
        return Err(miette::miette!(
            "the GUI frontend is not available in this runtime"
        ));
    }
    #[cfg(not(feature = "gui"))]
    if effective_frontend() == Frontend::Gui && args.ui {
        return Err(miette::miette!(
            "the GUI frontend is not available in this runtime"
        ));
    }
    #[cfg(feature = "console")]
    if effective_frontend() == Frontend::Console && console_interactive(&args) {
        return run_console_transition(action, args);
    }
    run_manifest_transition_noninteractive(action, args)
}

fn run_manifest_transition_noninteractive(
    action: LifecycleAction,
    args: ManifestCommand,
) -> miette::Result<()> {
    run_manifest_transition_with_policy(action, args, ExecutionPolicy::NonInteractive)
}

fn run_manifest_transition_with_policy(
    action: LifecycleAction,
    args: ManifestCommand,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    match zup_windows::EmbeddedBundle::open(&executable) {
        Ok(bundle) => {
            let build = embedded_target_plan(&bundle)?;
            let scope = if build.installer.install.scope == zup_core::InstallScope::Machine {
                SelectedScope::Machine
            } else {
                SelectedScope::from(args.scope)
            };
            run_embedded_transition_with_output(
                action,
                scope,
                EmbeddedTransitionOptions {
                    state: args.state_root,
                    enable: args.enable,
                    disable: args.disable,
                    install_directory: args.install_directory,
                    output: args.output.into(),
                    policy,
                },
            )
        }
        Err(error) if error.is_missing_resource() => {
            run_manifest_source_transition(action, args, policy)
        }
        Err(error) => Err(miette::miette!("installer package: {error}")),
    }
}

#[cfg(feature = "build")]
fn run_manifest_source_transition(
    action: LifecycleAction,
    args: ManifestCommand,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let scope = SelectedScope::from(args.scope);
    let manifest_path = args
        .manifest
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let loaded = load_single_project(&manifest_path, &args.target, &TargetOverrideArgs::default())?;

    let state_root = choose_state_root(args.state_root.clone(), scope)?;
    let config = loaded
        .selected_targets
        .first()
        .expect("single target selection");
    let target_plan = &loaded.build.targets[0];
    let installer = &target_plan.installer;
    let app_id = installer.app.id.clone();
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let allow_directory_override = installer.install.allow_directory_override;
    let mut request = zup_plan::PlanRequest::new(config.target.clone(), scope);
    if matches!(action, LifecycleAction::Repair { .. }) && args.install_directory.is_some() {
        return Err(miette::miette!(
            "repair uses the committed install location"
        ));
    }
    request.install_directory = if matches!(action, LifecycleAction::Repair { .. }) {
        allow_directory_override
            .then(|| persisted_install_directory(prior.as_ref()))
            .flatten()
    } else {
        choose_install_directory(
            args.install_directory.as_deref(),
            prior.as_ref(),
            allow_directory_override,
        )?
    };
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
        for component in &target_plan.installer.components {
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
    let install = zup_plan::plan(&loaded.build, &request).map_err(|error| match error {
        zup_plan::PlanError::PluginPlanningRequired { plugin_id } => miette::miette!(
            "active plugin `{plugin_id}` requires an embedded AOT package; source plugin JIT is disabled"
        ),
        error => miette::miette!("plan: {error}"),
    })?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        action,
        &app_id,
        scope,
        Some(&target),
        &state_root,
        installer.frontend,
    )
    .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
    let payload_root = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&config.source.directory);
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let bootstrap = prepare_bootstrap(target_plan, &install, &state_root, scope, None)?;
    let request = RuntimeRequest {
        target: target.target.clone(),
        app_id,
        app_version: target.app.version.clone(),
        scope,
        transaction_plan: execution,
        state_root,
        work_root,
        recovery_id: None,
        bootstrap,
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_path(payload_root, None)
        .map_err(|error| miette::miette!("payload source: {error}"))?;
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
    };
    if args.output == OutputArg::Human {
        execute_with_policy(prepared, policy)
    } else {
        execute_frontend(prepared, args.output.into(), action)
    }
}

#[cfg(not(feature = "build"))]
fn run_manifest_source_transition(
    _action: LifecycleAction,
    _args: ManifestCommand,
    _policy: ExecutionPolicy,
) -> miette::Result<()> {
    Err(miette::miette!(
        "source-manifest lifecycle mode is unavailable in a runtime-only zup build"
    ))
}

fn validate_downloaded_update(
    path: &Path,
    expected_app_id: &AppId,
    expected_version: &semver::Version,
    expected_target: &TargetTriple,
    expected_frontend: Frontend,
    scope: SelectedScope,
) -> miette::Result<()> {
    let target = zup_windows::read_pe_target(path)
        .map_err(|error| miette::miette!("downloaded update target: {error}"))?;
    if target != *expected_target {
        return Err(miette::miette!(
            "downloaded update target `{target}` does not match `{expected_target}`"
        ));
    }
    zup_windows::validate_pe_frontend(path, expected_frontend)
        .map_err(|error| miette::miette!("downloaded update frontend: {error}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(path)
        .map_err(|error| miette::miette!("downloaded update package: {error}"))?;
    let build = embedded_target_plan(&bundle)?;
    let installer = &build.installer;
    if &installer.target != expected_target {
        return Err(miette::miette!(
            "downloaded update package target `{}` does not match `{expected_target}`",
            installer.target
        ));
    }
    if &installer.app.id != expected_app_id {
        return Err(miette::miette!(
            "downloaded update application `{}` does not match `{}`",
            installer.app.id,
            expected_app_id
        ));
    }
    if &installer.app.version != expected_version {
        return Err(miette::miette!(
            "downloaded update version `{}` does not match `{expected_version}`",
            installer.app.version
        ));
    }
    if installer.frontend != expected_frontend {
        return Err(miette::miette!(
            "downloaded update frontend is {}, expected {expected_frontend}",
            installer.frontend
        ));
    }
    let scope_allowed = match scope {
        SelectedScope::User => installer.install.scope.allows_user(),
        SelectedScope::Machine => installer.install.scope.allows_machine(),
    };
    if !scope_allowed {
        return Err(miette::miette!(
            "downloaded update does not support the {scope} scope"
        ));
    }
    Ok(())
}

fn run_update(args: UpdateCommand) -> miette::Result<()> {
    let output: OutputFormat = args.output.into();
    let machine_install = output != OutputFormat::Human && args.command.is_none();
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_windows::EmbeddedBundle::open(&executable).map_err(|e| {
        miette::miette!("update configuration requires an installed zup package: {e}")
    })?;
    let build = embedded_target_plan(&bundle)?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| miette::miette!("updates are not configured in this package"))?;
    let mut scope = if installer.install.scope == zup_core::InstallScope::Machine {
        SelectedScope::Machine
    } else {
        args.scope
            .map(SelectedScope::from)
            .unwrap_or(SelectedScope::User)
    };
    let mut state_root = choose_state_root(args.state_root.clone(), scope)?;
    let mut ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|e| miette::miette!("installation ledger: {e}"))?;
    if ledger.is_none()
        && args.scope.is_none()
        && args.state_root.is_none()
        && scope == SelectedScope::User
    {
        scope = SelectedScope::Machine;
        state_root = choose_state_root(None, scope)?;
        ledger = zup_windows::InstallLedgerStore::new(&state_root)
            .load(&installer.app.id, scope)
            .map_err(|e| miette::miette!("installation ledger: {e}"))?;
    }
    let ledger =
        ledger.ok_or_else(|| miette::miette!("installation not found in selected scope"))?;
    let update_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        default_state_root(SelectedScope::User)?
    } else {
        state_root.clone()
    };
    let client = zup_update::Client::new(config, installer.app.id.as_str(), &update_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("update runtime: {e}"))?;
    if output == OutputFormat::Jsonl && !machine_install {
        println!(
            "{}",
            serde_json::to_string(&AutomationEvent::started(
                installer.app.id.as_str(),
                ledger.version.to_string(),
                "update",
            ))
            .map_err(|error| miette::miette!("output: {error}"))?
        );
        println!(
            "{}",
            serde_json::to_string(&AutomationEvent::Phase {
                state: "checking".into(),
            })
            .map_err(|error| miette::miette!("output: {error}"))?
        );
    } else if output == OutputFormat::Human && std::io::stderr().is_terminal() {
        eprintln!("Checking for updates…");
    }
    let result = runtime
        .block_on(client.check(&ledger.version))
        .map_err(|e| miette::miette!("update check: {e}"))?;
    match result {
        zup_update::CheckResult::UpToDate { current } => match output {
            OutputFormat::Human => println!("up to date ({current})"),
            OutputFormat::Json => {
                let mut result = AutomationResult::new(
                    ProcessOutcome::Success,
                    installer.app.id.as_str(),
                    current.to_string(),
                );
                result.scope = Some(scope);
                println!(
                    "{}",
                    result
                        .to_json()
                        .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
            OutputFormat::Jsonl => {
                if machine_install {
                    let started = AutomationEvent::started(
                        installer.app.id.as_str(),
                        current.to_string(),
                        "update",
                    );
                    println!(
                        "{}",
                        serde_json::to_string(&started)
                            .map_err(|error| miette::miette!("output: {error}"))?
                    );
                }
                println!(
                    "{}",
                    serde_json::to_string(&AutomationEvent::Completed {
                        outcome: ProcessOutcome::Success,
                    })
                    .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
        },
        zup_update::CheckResult::UpdateAvailable {
            current,
            available,
            target,
        } => {
            let should_install = args.command.is_none();
            if should_install
                && effective_frontend() == Frontend::Console
                && !args.non_interactive
                && !args.yes
                && output == OutputFormat::Human
                && std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && std::io::stderr().is_terminal()
            {
                #[cfg(feature = "console")]
                {
                    cliclack::set_theme(ZupTheme);
                    let confirmed = cliclack::confirm(format!("Install update {available}?"))
                        .initial_value(true)
                        .interact()
                        .map_err(|error| miette::miette!("prompt: {error}"))?;
                    if !confirmed {
                        let _ = cliclack::outro_cancel("Cancelled");
                        return Err(miette::miette!("cancelled"));
                    }
                }
                #[cfg(not(feature = "console"))]
                return Err(miette::miette!(
                    "confirmation is unavailable in this runtime"
                ));
            }
            if output == OutputFormat::Human {
                println!("update available: {current} → {available}");
            }
            let mut installed = false;
            if should_install {
                let downloaded = update_root
                    .join("updates")
                    .join("downloads")
                    .join(format!("Setup-{}.exe", uuid::Uuid::now_v7()));
                if output == OutputFormat::Human && std::io::stderr().is_terminal() {
                    eprintln!("Downloading and verifying update…");
                }
                runtime
                    .block_on(client.download(&target, &downloaded))
                    .map_err(|e| miette::miette!("verified update download: {e}"))?;
                validate_downloaded_update(
                    &downloaded,
                    &installer.app.id,
                    &available,
                    &installer.target,
                    installer.frontend,
                    scope,
                )?;
                let mut command = std::process::Command::new(&downloaded);
                command
                    .arg("upgrade")
                    .arg("--scope")
                    .arg(scope.to_string())
                    .arg("--state-root")
                    .arg(&state_root)
                    .arg("--yes");
                if output == OutputFormat::Human && effective_frontend() == Frontend::Gui {
                    command.arg("--ui");
                }
                if effective_frontend() == Frontend::Headless
                    || args.non_interactive
                    || output != OutputFormat::Human
                {
                    command.arg("--non-interactive");
                }
                if output != OutputFormat::Human {
                    command.arg("--output").arg(match output {
                        OutputFormat::Human => "human",
                        OutputFormat::Json => "json",
                        OutputFormat::Jsonl => "jsonl",
                    });
                }
                if output == OutputFormat::Human {
                    let status = command
                        .status()
                        .map_err(|e| miette::miette!("start verified update: {e}"))?;
                    if !status.success() {
                        return Err(miette::miette!("verified update exited with {status}"));
                    }
                    installed = true;
                } else {
                    use std::io::Write;
                    use std::process::Stdio;
                    let child = command
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|e| miette::miette!("start verified update: {e}"))?;
                    let child_output = child
                        .wait_with_output()
                        .map_err(|e| miette::miette!("wait for verified update: {e}"))?;
                    let has_output = !child_output.stdout.is_empty();
                    std::io::stdout()
                        .write_all(&child_output.stdout)
                        .map_err(|e| miette::miette!("write update output: {e}"))?;
                    std::io::stderr()
                        .write_all(&child_output.stderr)
                        .map_err(|e| miette::miette!("write update diagnostics: {e}"))?;
                    if !child_output.status.success() {
                        if has_output {
                            OUTPUT_FAILURE_EMITTED.store(true, Ordering::SeqCst);
                        }
                        return Err(miette::miette!(
                            "verified update exited with {}: {}",
                            child_output.status,
                            String::from_utf8_lossy(&child_output.stderr).trim()
                        ));
                    }
                    return Ok(());
                }
            }
            match output {
                OutputFormat::Human => {
                    if installed {
                        println!("update installed: {available}");
                    }
                }
                OutputFormat::Json => {
                    let mut result = AutomationResult::new(
                        ProcessOutcome::Success,
                        installer.app.id.as_str(),
                        available.to_string(),
                    );
                    result.scope = Some(scope);
                    result.message = Some(if installed {
                        format!("updated from {current}")
                    } else {
                        format!("update available from {current}")
                    });
                    println!(
                        "{}",
                        result
                            .to_json()
                            .map_err(|error| miette::miette!("output: {error}"))?
                    );
                }
                OutputFormat::Jsonl => {
                    println!(
                        "{}",
                        serde_json::to_string(&AutomationEvent::Completed {
                            outcome: ProcessOutcome::Success,
                        })
                        .map_err(|error| miette::miette!("output: {error}"))?
                    );
                }
            }
        }
    }
    Ok(())
}

fn resolve_uninstall_scope(
    app_id: &AppId,
    installer: Option<&zup_core::Installer>,
    requested: ScopeArg,
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    let allowed = installer.map_or(
        [SelectedScope::User, SelectedScope::Machine].as_slice(),
        |installer| match installer.install.scope {
            zup_core::InstallScope::User => &[SelectedScope::User][..],
            zup_core::InstallScope::Machine => &[SelectedScope::Machine][..],
            zup_core::InstallScope::Either => &[SelectedScope::User, SelectedScope::Machine][..],
        },
    );
    if installer.is_some_and(|installer| installer.install.scope == zup_core::InstallScope::Machine)
    {
        return Ok((
            SelectedScope::Machine,
            choose_state_root(state_root.map(Path::to_path_buf), SelectedScope::Machine)?,
        ));
    }
    if matches!(requested, ScopeArg::User | ScopeArg::Machine) {
        let scope = SelectedScope::from(requested);
        if !allowed.contains(&scope) {
            return Err(miette::miette!(
                "the selected scope is not supported by this application"
            ));
        }
        return Ok((
            scope,
            choose_state_root(state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    let mut found = Vec::new();
    for scope in allowed.iter().copied() {
        let candidate_root = choose_state_root(state_root.map(Path::to_path_buf), scope)?;
        if zup_windows::InstallLedgerStore::new(&candidate_root)
            .load(app_id, scope)
            .map_err(|error| miette::miette!("ledger: {error}"))?
            .is_some()
        {
            found.push((scope, candidate_root));
        }
    }
    if found.len() > 1 {
        return Err(miette::miette!(
            "installation exists in both user and machine scopes; choose --scope"
        ));
    }
    if let Some(result) = found.pop() {
        return Ok(result);
    }
    let scope = SelectedScope::User;
    Ok((
        scope,
        choose_state_root(state_root.map(Path::to_path_buf), scope)?,
    ))
}

fn run_uninstall(args: UninstallCommand) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    #[cfg(feature = "gui")]
    if effective_frontend() == Frontend::Gui && args.ui {
        let bundle = zup_windows::EmbeddedBundle::open(&executable)
            .map_err(|error| miette::miette!("installer package: {error}"))?;
        return run_graphical_frontend(executable, &bundle, true, false);
    }
    #[cfg(feature = "gui")]
    if args.ui {
        return Err(miette::miette!(
            "the GUI frontend is not available in this runtime"
        ));
    }
    #[cfg(not(feature = "gui"))]
    if effective_frontend() == Frontend::Gui && args.ui {
        return Err(miette::miette!(
            "the GUI frontend is not available in this runtime"
        ));
    }
    if executable
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\maintenance\\")
    {
        let mut child = launch_uninstall_runner(&executable, &args, false)?;
        if args.non_interactive || args.output != OutputArg::Human {
            let status = child
                .wait()
                .map_err(|error| miette::miette!("wait for uninstall runner: {error}"))?;
            if !status.success() {
                return Err(miette::miette!("uninstall runner exited with {status}"));
            }
        }
        return Ok(());
    }
    #[cfg(feature = "console")]
    if effective_frontend() == Frontend::Console
        && !args.non_interactive
        && !args.yes
        && args.output == OutputArg::Human
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
    {
        let confirmed = cliclack::confirm("Uninstall this application?")
            .initial_value(false)
            .interact()
            .map_err(|error| miette::miette!("prompt: {error}"))?;
        if !confirmed {
            let _ = cliclack::outro_cancel("Cancelled");
            return Err(miette::miette!("cancelled"));
        }
    }
    let embedded = match zup_windows::EmbeddedBundle::open(&executable) {
        Ok(bundle) => Some(bundle),
        Err(error) if error.is_missing_resource() => None,
        Err(error) => return Err(miette::miette!("installer package: {error}")),
    };
    let embedded_target = embedded.as_ref().map(embedded_target_plan).transpose()?;
    let app_id = match args.app_id.as_deref() {
        Some(id) => AppId::new(id).map_err(|error| miette::miette!("app ID: {error}"))?,
        None => embedded_target
            .as_ref()
            .ok_or_else(|| miette::miette!("installer package unavailable"))?
            .installer
            .app
            .id
            .clone(),
    };
    let (scope, state_root) = resolve_uninstall_scope(
        &app_id,
        embedded_target.as_ref().map(|build| &build.installer),
        args.scope.unwrap_or(ScopeArg::Either),
        args.state_root.as_deref(),
    )?;
    let output = args.output;
    let policy = if args.non_interactive
        || args.yes
        || args.output != OutputArg::Human
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
    };
    if embedded.is_some() {
        let result = run_embedded_transition_with_output(
            LifecycleAction::Uninstall,
            scope,
            EmbeddedTransitionOptions {
                state: Some(state_root.clone()),
                enable: Vec::new(),
                disable: Vec::new(),
                install_directory: None,
                output: output.into(),
                policy,
            },
        );
        if result.is_ok() {
            remove_uninstall_lock(&state_root, &app_id, scope)?;
            zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
                .map_err(|error| miette::miette!("cleanup payload overlays: {error}"))?;
        }
        return result;
    }
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        LifecycleAction::Uninstall,
        &app_id,
        scope,
        None,
        &state_root,
        embedded_target
            .as_ref()
            .map_or(Frontend::Gui, |build| build.installer.frontend),
    )
    .map_err(|error| miette::miette!("uninstall plan: {error}"))?;
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let target = embedded_target
        .as_ref()
        .map(|build| build.installer.target.clone())
        .unwrap_or_else(|| ledger.target.clone());
    if target != ledger.target {
        return Err(miette::miette!(
            "embedded package target `{target}` does not match ledger target `{}`",
            ledger.target
        ));
    }
    let payload_root =
        std::env::current_dir().map_err(|error| miette::miette!("working directory: {error}"))?;
    let request = RuntimeRequest {
        target,
        app_id: app_id.clone(),
        app_version: ledger.version,
        scope,
        transaction_plan: execution,
        state_root: state_root.clone(),
        work_root,
        recovery_id: None,
        bootstrap: None,
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_path(payload_root, None)
        .map_err(|error| miette::miette!("payload source: {error}"))?;
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
    };
    let result = if output == OutputArg::Human {
        execute_with_policy(prepared, policy)
    } else {
        execute_frontend(prepared, output.into(), LifecycleAction::Uninstall)
    };
    if result.is_ok() {
        remove_uninstall_lock(&state_root, &app_id, scope)?;
        zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
            .map_err(|error| miette::miette!("cleanup payload overlays: {error}"))?;
    }
    result
}

fn remove_uninstall_lock(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
) -> miette::Result<()> {
    let scope = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let key = zup_windows::InstallationLock::lock_key(app_id.as_str(), scope);
    zup_windows::InstallationLock::remove_if_unheld(state_root, &key)
        .map_err(|error| miette::miette!("remove uninstall lock: {error}"))
}

fn launch_uninstall_runner(
    executable: &Path,
    args: &UninstallCommand,
    ui: bool,
) -> miette::Result<std::process::Child> {
    let temporary =
        std::env::temp_dir().join(format!("zup-uninstall-{}.exe", uuid::Uuid::now_v7()));
    zup_windows::copy_new_durable(executable, &temporary)
        .map_err(|error| miette::miette!("prepare uninstall runner: {error}"))?;
    let mut command = std::process::Command::new(&temporary);
    command
        .arg("__uninstall_runner")
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
    if ui {
        command.arg("--ui");
    }
    if let Some(scope) = args.scope {
        command.arg("--scope").arg(match scope {
            ScopeArg::User => "user",
            ScopeArg::Machine => "machine",
            ScopeArg::Either => "either",
        });
    }
    if let Some(value) = &args.app_id {
        command.arg("--app-id").arg(value);
    }
    if let Some(value) = &args.state_root {
        command.arg("--state-root").arg(value);
    }
    if let Some(value) = &args.work_root {
        command.arg("--work-root").arg(value);
    }
    if args.non_interactive {
        command.arg("--non-interactive");
    }
    if args.yes {
        command.arg("--yes");
    }
    if args.output != OutputArg::Human {
        command.arg("--output").arg(match args.output {
            OutputArg::Human => "human",
            OutputArg::Json => "json",
            OutputArg::Jsonl => "jsonl",
        });
    }
    command
        .spawn()
        .map_err(|error| miette::miette!("start uninstall runner: {error}"))
}

fn schedule_runner_cleanup(path: &Path) {
    let path = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue",
        std::process::id(),
        path
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn();
}

fn run_recover(args: RecoverCommand) -> miette::Result<()> {
    use zup_transaction::TransactionStore;
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = match zup_windows::EmbeddedBundle::open(&executable) {
        Ok(bundle) => Some(bundle),
        Err(error) if error.is_missing_resource() => None,
        Err(error) => return Err(miette::miette!("installer package: {error}")),
    };
    let embedded_target = bundle.as_ref().map(embedded_target_plan).transpose()?;
    let scope = embedded_target.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |build| default_install_scope(build.installer.install.scope),
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
            std::env::current_dir().unwrap_or_else(|_| state_root.clone())
        }
    });
    let target = embedded_target
        .as_ref()
        .map(|build| build.installer.target.clone())
        .unwrap_or_else(|| record.target.clone());
    if target != record.target {
        return Err(miette::miette!(
            "embedded package target `{target}` does not match recovery target `{}`",
            record.target
        ));
    }
    let backend = zup_windows::WindowsRuntimeBackend::for_recovery(
        payload_root,
        &state_root,
        record.scope,
        id,
    )
    .map_err(|error| miette::miette!("payload recovery: {error}"))?;
    let request = RuntimeRequest {
        target,
        app_id: record.app_id,
        app_version: record.app_version,
        scope: record.scope,
        transaction_plan: record.plan.clone(),
        state_root,
        work_root,
        recovery_id: Some(id),
        bootstrap: None,
    };
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
    };
    if args.output == OutputArg::Human {
        execute(prepared)
    } else {
        execute_frontend(
            prepared,
            args.output.into(),
            LifecycleAction::Repair { force_files: false },
        )
    }
}

fn execute(prepared: PreparedRuntime) -> miette::Result<()> {
    execute_with_policy(prepared, ExecutionPolicy::NonInteractive)
}

fn execute_with_policy(prepared: PreparedRuntime, policy: ExecutionPolicy) -> miette::Result<()> {
    let drifted: Vec<String> = prepared
        .request
        .transaction_plan
        .retired_keys
        .iter()
        .map(|key| format!("{key:?}"))
        .collect();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let cancel = zup_runtime::CancellationHandle::new();
    let outcome = execute_with_control_signal(prepared, cancel, events, policy)?;
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

#[cfg(feature = "gui")]
fn execute_with_control(
    prepared: PreparedRuntime,
    cancel: zup_runtime::CancellationHandle,
    events: tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>,
) -> miette::Result<InstallOutcome> {
    execute_with_control_signal(prepared, cancel, events, ExecutionPolicy::Interactive)
}

fn execute_with_control_signal(
    prepared: PreparedRuntime,
    cancel: zup_runtime::CancellationHandle,
    events: tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>,
    policy: ExecutionPolicy,
) -> miette::Result<InstallOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            prepared.backend.cleanup_overlay(&prepared.request);
            miette::miette!("runtime: {error}")
        })?;
    runtime
        .block_on(async move {
            let PreparedRuntime { request, backend } = prepared;
            let operation = zup_windows::run_install_control_with_policy(
                &backend,
                request,
                cancel.clone(),
                events.clone(),
                policy,
            );
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?;
                    cancel.cancel();
                    let _ = events.send(zup_runtime::RuntimeEvent::StateChanged {
                        state: zup_runtime::RuntimeState::Cancelled,
                    });
                    operation.await
                }
            }
        })
        .map_err(|error| miette::miette!("install session: {error}"))
}

fn lifecycle_action_name(action: LifecycleAction) -> &'static str {
    match action {
        LifecycleAction::Install => "install",
        LifecycleAction::Upgrade => "upgrade",
        LifecycleAction::Modify => "modify",
        LifecycleAction::Repair { .. } => "repair",
        LifecycleAction::Uninstall => "uninstall",
    }
}

fn is_terminal_runtime_event(event: &zup_runtime::RuntimeEvent) -> bool {
    matches!(
        event,
        zup_runtime::RuntimeEvent::Completed { .. } | zup_runtime::RuntimeEvent::Failed { .. }
    )
}

fn process_outcome(outcome: &InstallOutcome) -> ProcessOutcome {
    match outcome {
        InstallOutcome::RebootRequired { .. } => ProcessOutcome::RebootRequired,
        InstallOutcome::Committed => ProcessOutcome::Success,
        InstallOutcome::Cancelled => ProcessOutcome::Cancelled,
        InstallOutcome::RecoveryRequired => ProcessOutcome::RecoveryRequired,
        InstallOutcome::RolledBack => ProcessOutcome::Failure,
        InstallOutcome::Failed(message) => ProcessOutcome::from_message(message),
    }
}

fn runtime_state_name(state: zup_runtime::RuntimeState) -> &'static str {
    match state {
        zup_runtime::RuntimeState::Preparing => "preparing",
        zup_runtime::RuntimeState::CheckingPrerequisites => "checking_prerequisites",
        zup_runtime::RuntimeState::InstallingPrerequisites => "installing_prerequisites",
        zup_runtime::RuntimeState::RebootRequired => "reboot_required",
        zup_runtime::RuntimeState::WaitingForAuthorization => "waiting_for_authorization",
        zup_runtime::RuntimeState::ConnectingWorker => "connecting_worker",
        zup_runtime::RuntimeState::Executing => "executing",
        zup_runtime::RuntimeState::RollingBack => "rolling_back",
        zup_runtime::RuntimeState::Completed => "completed",
        zup_runtime::RuntimeState::Cancelled => "cancelling",
        zup_runtime::RuntimeState::Failed => "failed",
    }
}

fn automation_events(event: &zup_runtime::RuntimeEvent) -> Vec<AutomationEvent> {
    match event {
        zup_runtime::RuntimeEvent::StateChanged { state } => {
            if *state == zup_runtime::RuntimeState::Cancelled {
                vec![AutomationEvent::Cancelling {
                    state: "safe_boundary".into(),
                }]
            } else {
                vec![AutomationEvent::Phase {
                    state: runtime_state_name(*state).into(),
                }]
            }
        }
        zup_runtime::RuntimeEvent::WaitingForAuthorization => vec![AutomationEvent::Phase {
            state: "waiting_for_authorization".into(),
        }],
        zup_runtime::RuntimeEvent::WorkerConnected => vec![AutomationEvent::Phase {
            state: "worker_connected".into(),
        }],
        zup_runtime::RuntimeEvent::PreflightStarted => vec![AutomationEvent::Phase {
            state: "preflight".into(),
        }],
        zup_runtime::RuntimeEvent::ResourceBlocked { detail, pids } => {
            vec![AutomationEvent::blocked_with_processes(
                detail,
                pids.clone(),
            )]
        }
        zup_runtime::RuntimeEvent::StagingStarted { id } => {
            vec![AutomationEvent::Phase {
                state: format!("staging:{id}"),
            }]
        }
        zup_runtime::RuntimeEvent::StagingProgress { id, detail } => {
            vec![AutomationEvent::Phase {
                state: format!("staging:{id}:{detail}"),
            }]
        }
        zup_runtime::RuntimeEvent::OperationStarted { id } => vec![AutomationEvent::Phase {
            state: format!("operation:{id}"),
        }],
        zup_runtime::RuntimeEvent::Progress {
            completed,
            total,
            action,
        } => vec![AutomationEvent::progress(
            &zup_presentation::ProgressPresentation::new(*completed, *total, action),
        )],
        zup_runtime::RuntimeEvent::PrerequisiteCheck {
            id,
            name,
            satisfied,
            version,
        } => vec![AutomationEvent::PrerequisiteCheck {
            id: id.clone(),
            name: name.clone(),
            satisfied: *satisfied,
            version: version.clone(),
        }],
        zup_runtime::RuntimeEvent::PrerequisiteDownload {
            id,
            completed,
            total,
        } => vec![AutomationEvent::PrerequisiteDownload {
            id: id.clone(),
            completed: *completed,
            total: *total,
        }],
        zup_runtime::RuntimeEvent::PrerequisiteInstall { id, name } => {
            vec![AutomationEvent::PrerequisiteInstall {
                id: id.clone(),
                name: name.clone(),
            }]
        }
        zup_runtime::RuntimeEvent::RebootRequired { id, exit_code } => {
            vec![AutomationEvent::RebootRequired {
                id: id.clone(),
                exit_code: *exit_code,
            }]
        }
        zup_runtime::RuntimeEvent::RollingBack => vec![AutomationEvent::Phase {
            state: "rolling_back".into(),
        }],
        zup_runtime::RuntimeEvent::Completed { outcome } => {
            let outcome = if outcome == "committed" {
                ProcessOutcome::Success
            } else {
                ProcessOutcome::from_message(outcome)
            };
            vec![AutomationEvent::Completed { outcome }]
        }
        zup_runtime::RuntimeEvent::Failed { kind, message } => {
            let outcome = ProcessOutcome::from_message(message);
            vec![AutomationEvent::Failed {
                outcome,
                code: outcome.code(),
                message: message.clone(),
                diagnostic: Some(zup_presentation::DiagnosticPresentation::from_message(
                    message,
                    *kind == "recovery_required",
                )),
            }]
        }
        zup_runtime::RuntimeEvent::LogPath { .. } => Vec::new(),
    }
}

fn execute_frontend(
    prepared: PreparedRuntime,
    output: OutputFormat,
    action: LifecycleAction,
) -> miette::Result<()> {
    let request = prepared.request.clone();
    let application = request.app_id.to_string();
    let version = request.app_version.to_string();
    let scope = Some(request.scope);
    let install_directory = request
        .transaction_plan
        .install_directory
        .as_ref()
        .map(ToString::to_string);
    let drifted: Vec<String> = request
        .transaction_plan
        .retired_keys
        .iter()
        .map(|key| format!("{key:?}"))
        .collect();
    let (events, _) = tokio::sync::broadcast::channel(256);
    let mut receiver = events.subscribe();
    if output == OutputFormat::Jsonl {
        let started =
            AutomationEvent::started(&application, &version, lifecycle_action_name(action));
        println!(
            "{}",
            serde_json::to_string(&started).map_err(|error| miette::miette!("output: {error}"))?
        );
    }
    let log_path = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let pump_log_path = log_path.clone();
    let jsonl = output == OutputFormat::Jsonl;
    let pump = std::thread::spawn(move || {
        loop {
            let event = match receiver.blocking_recv() {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            };
            if let zup_runtime::RuntimeEvent::LogPath { path } = &event {
                *pump_log_path.lock().expect("log path state") = Some(path.clone());
            }
            for output_event in automation_events(&event) {
                if jsonl && let Ok(line) = serde_json::to_string(&output_event) {
                    println!("{line}");
                }
            }
            if is_terminal_runtime_event(&event) {
                return true;
            }
        }
    });
    let cancel = zup_runtime::CancellationHandle::new();
    let outcome =
        execute_with_control_signal(prepared, cancel, events, ExecutionPolicy::NonInteractive);
    let terminal_seen = pump.join().unwrap_or(false);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            if output != OutputFormat::Human {
                let process = ProcessOutcome::from_message(&error.to_string());
                match output {
                    OutputFormat::Json => {
                        let mut result =
                            AutomationResult::new(process, application.clone(), version.clone());
                        result.scope = scope;
                        result.install_directory = install_directory.clone();
                        result.log_path = log_path.lock().expect("log path state").clone();
                        result.drift = drifted.clone();
                        result.message = Some(error.to_string());
                        println!(
                            "{}",
                            result.to_json().map_err(|output_error| miette::miette!(
                                "output: {output_error}"
                            ))?
                        );
                    }
                    OutputFormat::Jsonl => {
                        if !terminal_seen {
                            let event = AutomationEvent::Failed {
                                outcome: process,
                                code: process.code(),
                                message: error.to_string(),
                                diagnostic: Some(
                                    zup_presentation::DiagnosticPresentation::from_message(
                                        &error.to_string(),
                                        process == ProcessOutcome::RecoveryRequired,
                                    ),
                                ),
                            };
                            println!(
                                "{}",
                                serde_json::to_string(&event).map_err(
                                    |output_error| miette::miette!("output: {output_error}")
                                )?
                            );
                        }
                    }
                    OutputFormat::Human => {}
                }
                OUTPUT_FAILURE_EMITTED.store(true, Ordering::SeqCst);
            }
            return Err(error);
        }
    };
    let process_outcome = process_outcome(&outcome);
    match output {
        OutputFormat::Human => {
            if outcome == InstallOutcome::Committed {
                println!("committed");
                for key in drifted {
                    eprintln!("left drifted resource untouched: {key}");
                }
                Ok(())
            } else {
                Err(miette::miette!("transaction: {outcome:?}"))
            }
        }
        OutputFormat::Json => {
            let mut result = AutomationResult::new(process_outcome, application, version);
            result.scope = scope;
            result.install_directory = install_directory;
            result.log_path = log_path.lock().expect("log path state").clone();
            result.drift = drifted;
            if process_outcome != ProcessOutcome::Success {
                result.message = Some(format!("{outcome:?}"));
            }
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| miette::miette!("output: {error}"))?
            );
            if process_outcome == ProcessOutcome::Success {
                Ok(())
            } else {
                OUTPUT_FAILURE_EMITTED.store(true, Ordering::SeqCst);
                Err(miette::miette!("transaction: {outcome:?}"))
            }
        }
        OutputFormat::Jsonl => {
            if !terminal_seen {
                let event = AutomationEvent::Completed {
                    outcome: process_outcome,
                };
                println!(
                    "{}",
                    serde_json::to_string(&event)
                        .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
            if !matches!(outcome, InstallOutcome::Committed) {
                OUTPUT_FAILURE_EMITTED.store(true, Ordering::SeqCst);
                return Err(miette::miette!("transaction: {outcome:?}"));
            }
            Ok(())
        }
    }
}

#[cfg(feature = "console")]
fn execute_console(prepared: PreparedRuntime, action: LifecycleAction) -> miette::Result<()> {
    let mut pending = Some(prepared);
    while let Some(prepared) = pending.take() {
        let retry_request = prepared.clone();
        let result = execute_console_once(prepared, action);
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                retry_request
                    .backend
                    .cleanup_overlay(&retry_request.request);
                return Err(error);
            }
        };
        match outcome {
            InstallOutcome::Committed => {
                println!("done");
                return Ok(());
            }
            InstallOutcome::Cancelled => return Err(miette::miette!("cancelled")),
            InstallOutcome::Failed(message) if message == "blocked by running applications" => {
                let choice = cliclack::select("Installation blocked")
                    .item("retry", "Retry", "after closing the listed applications")
                    .item("cancel", "Cancel", "stop without changing the installation")
                    .initial_value("retry")
                    .interact();
                let choice = match choice {
                    Ok(choice) => choice,
                    Err(error) => {
                        retry_request
                            .backend
                            .cleanup_overlay(&retry_request.request);
                        return Err(miette::miette!("prompt: {error}"));
                    }
                };
                if choice == "retry" {
                    pending = Some(retry_request);
                } else {
                    retry_request
                        .backend
                        .cleanup_overlay(&retry_request.request);
                    return Err(miette::miette!("cancelled"));
                }
            }
            other => return Err(miette::miette!("transaction: {other:?}")),
        }
    }
    Err(miette::miette!("cancelled"))
}

#[cfg(feature = "console")]
fn execute_console_once(
    prepared: PreparedRuntime,
    action: LifecycleAction,
) -> miette::Result<InstallOutcome> {
    let request = prepared.request.clone();
    println!("{} {}", lifecycle_action_name(action), request.app_id);
    let total = request.transaction_plan.total_work();
    let progress = indicatif::ProgressBar::new(total);
    progress.set_style(
        indicatif::ProgressStyle::with_template("  {msg:.<28} {bar:30.cyan/blue} {percent:>3}%")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
    );
    let (events, _) = tokio::sync::broadcast::channel(256);
    let mut receiver = events.subscribe();
    let pump_progress = progress.clone();
    let pump = std::thread::spawn(move || {
        loop {
            let event = match receiver.blocking_recv() {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let terminal = is_terminal_runtime_event(&event);
            match &event {
                zup_runtime::RuntimeEvent::Progress {
                    completed,
                    total,
                    action,
                } => {
                    if *total > 0 {
                        pump_progress.set_length(*total);
                    }
                    pump_progress.set_position((*completed).min((*total).max(1)));
                    pump_progress.set_message(action.clone());
                }
                zup_runtime::RuntimeEvent::ResourceBlocked { detail, .. } => {
                    eprintln!("installation blocked by running applications\n{detail}");
                }
                zup_runtime::RuntimeEvent::Failed { message, .. } => {
                    eprintln!("{message}");
                }
                zup_runtime::RuntimeEvent::StateChanged { state }
                    if *state == zup_runtime::RuntimeState::Cancelled =>
                {
                    println!("Cancelling safely…");
                }
                zup_runtime::RuntimeEvent::StateChanged { .. } => {}
                _ => {}
            }
            if terminal {
                break;
            }
        }
    });
    let cancel = zup_runtime::CancellationHandle::new();
    let prepared = prepared.with_overlay_policy(zup_windows::OverlayPolicy::RetainOnBlocked);
    let result =
        execute_with_control_signal(prepared, cancel, events, ExecutionPolicy::Interactive);
    progress.finish_and_clear();
    let _ = pump.join();
    result
}

#[cfg(feature = "gui")]
fn run_graphical_frontend(
    executable: PathBuf,
    bundle: &zup_windows::EmbeddedBundle,
    force_maintenance: bool,
    auto_uninstall: bool,
) -> miette::Result<()> {
    let build = embedded_target_plan(bundle)?;
    let installer = &build.installer;
    let app_id = &installer.app.id;
    let scopes = match installer.install.scope {
        zup_core::InstallScope::User => vec![SelectedScope::User],
        zup_core::InstallScope::Machine => vec![SelectedScope::Machine],
        zup_core::InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };
    let maintenance_launch = force_maintenance || is_maintenance_executable(&executable);
    let installed = scopes.iter().find_map(|scope| {
        let state = choose_state_root(None, *scope).ok()?;
        let ledger = zup_windows::InstallLedgerStore::new(state)
            .load(app_id, *scope)
            .ok()??;
        Some((*scope, ledger))
    });
    let selected_scope = installed.as_ref().map_or(scopes[0], |(scope, _)| *scope);
    let components = installer
        .components
        .iter()
        .map(|component| zup_ui::ComponentOption {
            id: component.id.clone(),
            name: component.name.to_string(),
            description: component.description.clone(),
            required: component.required,
            selected: installed
                .as_ref()
                .map_or(component.default || component.required, |(_, ledger)| {
                    ledger.selected_components.contains(&component.id)
                }),
        })
        .collect::<Vec<_>>();
    let identity = zup_ui::ProductIdentity {
        name: installer.app.name.to_string(),
        publisher: installer.app.publisher.as_ref().map(ToString::to_string),
        version: installer.app.version.to_string(),
        description: installer.app.description.clone(),
    };
    let mut initial_request = zup_plan::PlanRequest::new(installer.target.clone(), selected_scope);
    if let Some((_, ledger)) = installed.as_ref() {
        for component in &installer.components {
            if ledger.selected_components.contains(&component.id) {
                initial_request
                    .components
                    .enable
                    .insert(component.id.clone());
            } else if !component.required {
                initial_request
                    .components
                    .disable
                    .insert(component.id.clone());
            }
        }
    } else {
        for component in &installer.components {
            if component.default || component.required {
                initial_request
                    .components
                    .enable
                    .insert(component.id.clone());
            } else {
                initial_request
                    .components
                    .disable
                    .insert(component.id.clone());
            }
        }
    }
    if let Some(path) = persisted_install_directory(installed.as_ref().map(|(_, ledger)| ledger)) {
        initial_request.install_directory = Some(path);
    }
    let planning_build = zup_plan::BuildPlan {
        targets: vec![build.clone()],
    };
    let preview = zup_plan::plan(&planning_build, &initial_request)
        .ok()
        .map(|install| {
            let mut preview = zup_presentation::PlanPreview::from_install_plan(&install);
            if let Ok(target) = zup_windows::resolve_target(
                &install,
                &zup_windows::WindowsTargetContext::new(selected_scope),
            ) {
                preview.install_directory = target.install_directory.to_string();
                preview.estimated_bytes = target.summary.install_bytes;
                preview.requires_authorization = target.summary.requires_authorization;
            }
            preview
        });
    let install_directory = preview
        .as_ref()
        .map(|preview| preview.install_directory.clone())
        .or_else(|| {
            installed
                .as_ref()
                .and_then(|(_, ledger)| ledger.install_directory.as_ref().map(ToString::to_string))
        });
    let estimated_bytes = preview
        .as_ref()
        .map_or(0, |preview| preview.estimated_bytes);
    let requires_authorization = preview
        .as_ref()
        .is_some_and(|preview| preview.requires_authorization);
    let surface = if maintenance_launch {
        let (_scope, ledger) = installed
            .as_ref()
            .ok_or_else(|| miette::miette!("installed application was not found"))?;
        zup_ui::Surface::Maintenance {
            identity,
            installed_version: ledger.version.to_string(),
            components: components
                .iter()
                .cloned()
                .map(|mut component| {
                    component.selected = ledger.selected_components.contains(&component.id);
                    component
                })
                .collect(),
            updates_enabled: installer.updates.is_some(),
            scope: selected_scope,
            install_directory,
            health: zup_presentation::InstallationHealth {
                state: "Ready".into(),
                summary: "Up to date".into(),
                drift_count: 0,
            },
        }
    } else {
        zup_ui::Surface::Installer {
            identity,
            install: zup_ui::InstallModel {
                existing_version: installed
                    .as_ref()
                    .map(|(_, ledger)| ledger.version.to_string()),
                scopes: if installed.is_some() {
                    vec![selected_scope]
                } else {
                    scopes
                },
                selected_scope,
                components,
                install_directory: install_directory.clone(),
                allow_directory_override: installer.install.allow_directory_override,
                estimated_bytes,
                requires_authorization,
                preview: preview.clone(),
            },
        }
    };
    let (commands_tx, commands_rx) = std::sync::mpsc::channel();
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let state = installed
        .map(|(scope, ledger)| {
            choose_state_root(None, scope).map(|state| (scope, state, ledger.version))
        })
        .transpose()?;
    let backend_exe = executable.clone();
    let package_version = installer.app.version.clone();
    std::thread::Builder::new()
        .name("zup-ui-runtime".into())
        .spawn(move || {
            ui_backend(
                backend_exe,
                selected_scope,
                state,
                package_version,
                commands_rx,
                events_tx,
            )
        })
        .map_err(|error| miette::miette!("start UI runtime bridge: {error}"))?;
    if auto_uninstall {
        commands_tx
            .send(zup_ui::UiCommand::ConfirmUninstall)
            .map_err(|error| miette::miette!("start uninstall session: {error}"))?;
    }
    zup_ui::run_with_branding(surface, commands_tx, events_rx, installer.ui.clone());
    Ok(())
}

#[cfg(feature = "gui")]
fn ui_backend(
    executable: PathBuf,
    default_scope: SelectedScope,
    installed: Option<(SelectedScope, PathBuf, semver::Version)>,
    package_version: semver::Version,
    commands: std::sync::mpsc::Receiver<zup_ui::UiCommand>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
) {
    use std::sync::{Arc, Mutex};
    use zup_ui::{UiCommand as C, UiEvent as E};

    let cancel_slot = Arc::new(Mutex::new(None::<zup_runtime::CancellationHandle>));
    let retry_slot = Arc::new(Mutex::new(None::<RetryIntent>));
    let log_slot = Arc::new(Mutex::new(None::<String>));
    let bridge = UiOperationBridge {
        cancel_slot: cancel_slot.clone(),
        retry_slot: retry_slot.clone(),
        log_slot: log_slot.clone(),
        events: events.clone(),
    };
    for command in commands {
        match command {
            C::Cancel => {
                if let Some(cancel) = cancel_slot.lock().expect("cancel state").as_ref() {
                    cancel.cancel();
                    let _ = events.send(E::CancellationWaiting);
                } else {
                    let _ = events.send(E::OperationFinished(InstallOutcome::Cancelled));
                }
            }
            C::Uninstall => {
                let _ = events.send(E::ConfirmUninstall);
            }
            C::DismissUninstall => {
                let _ = events.send(E::DismissUninstall);
            }
            C::Close => {
                let _ = events.send(E::Quit);
            }
            C::OpenLog => {
                if let Some(path) = log_slot.lock().expect("log state").clone() {
                    open_log_path(&path);
                } else {
                    let _ = events.send(E::LogPath(session_log_path().display().to_string()));
                }
            }
            C::CopyDiagnostics => {
                let summary = diagnostic_summary(log_slot.lock().expect("log state").as_deref());
                copy_to_clipboard(&summary);
            }
            C::Update => {
                let exe = executable.clone();
                let events = events.clone();
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                let current = installed
                    .as_ref()
                    .map_or_else(|| "unknown".into(), |(_, _, version)| version.to_string());
                std::thread::spawn(move || {
                    let status_events = events.clone();
                    let result = update_from_ui(&exe, scope, move |state| {
                        let _ = status_events.send(zup_ui::UiEvent::UpdateStatus(
                            zup_presentation::UpdatePresentation {
                                channel: None,
                                state: state.into(),
                                current: None,
                                available: None,
                            },
                        ));
                    });
                    match result {
                        Ok(Some((current, available))) => {
                            let _ = events.send(E::UpdateAvailable { current, available });
                        }
                        Ok(None) => {
                            let _ = events.send(E::UpToDate { current });
                        }
                        Err(message) => {
                            let _ = events.send(E::Error {
                                message,
                                recovery_required: false,
                            });
                        }
                    }
                });
            }
            C::Preview {
                scope,
                components,
                install_directory,
            } => {
                let events = events.clone();
                let executable = executable.clone();
                let installed_version = installed.as_ref().map(|(_, _, version)| version.clone());
                std::thread::spawn(move || {
                    if let Err(error) = request_ui_preview(
                        executable,
                        scope,
                        components,
                        install_directory.map(PathBuf::from),
                        installed_version,
                        &events,
                    ) {
                        let _ = events.send(zup_ui::UiEvent::Error {
                            message: error.to_string(),
                            recovery_required: false,
                        });
                    }
                });
            }
            C::Install {
                scope,
                components,
                install_directory,
            } => {
                let action = resolve_interactive_action(
                    LifecycleAction::Install,
                    installed.as_ref().map(|(_, _, version)| version),
                    &package_version,
                );
                let action = match action {
                    Ok(action) => action,
                    Err(error) => {
                        let _ = events.send(zup_ui::UiEvent::Error {
                            message: error.to_string(),
                            recovery_required: false,
                        });
                        return;
                    }
                };
                start_ui_transition(
                    executable.clone(),
                    scope,
                    action,
                    components,
                    install_directory.map(PathBuf::from),
                    false,
                    bridge.clone(),
                );
            }
            C::Modify { components } => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                start_ui_transition(
                    executable.clone(),
                    scope,
                    LifecycleAction::Modify,
                    components,
                    None,
                    false,
                    bridge.clone(),
                );
            }
            C::Repair => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                let components = installed_components(&executable, scope).unwrap_or_default();
                start_ui_transition(
                    executable.clone(),
                    scope,
                    LifecycleAction::Repair { force_files: false },
                    components,
                    None,
                    false,
                    bridge.clone(),
                );
            }
            C::ConfirmUninstall => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                if is_maintenance_executable(&executable) {
                    let Some((scope, state_root, _)) = installed.as_ref() else {
                        let _ = events.send(E::Error {
                            message: "Installed application was not found".into(),
                            recovery_required: false,
                        });
                        continue;
                    };
                    let app_id = match zup_windows::EmbeddedBundle::open(&executable) {
                        Ok(bundle) => match embedded_target_plan(&bundle) {
                            Ok(build) => build.installer.app.id.to_string(),
                            Err(error) => {
                                let _ = events.send(E::Error {
                                    message: format!(
                                        "The maintenance package could not be read: {error}"
                                    ),
                                    recovery_required: false,
                                });
                                continue;
                            }
                        },
                        Err(error) => {
                            let _ = events.send(E::Error {
                                message: format!(
                                    "The maintenance package could not be read: {error}"
                                ),
                                recovery_required: false,
                            });
                            continue;
                        }
                    };
                    let args = UninstallCommand {
                        ui: true,
                        app_id: Some(app_id),
                        scope: Some(match scope {
                            SelectedScope::User => ScopeArg::User,
                            SelectedScope::Machine => ScopeArg::Machine,
                        }),
                        state_root: Some(state_root.clone()),
                        work_root: None,
                        output: OutputArg::Human,
                        non_interactive: false,
                        yes: true,
                    };
                    match launch_uninstall_runner(&executable, &args, true).map(|_| ()) {
                        Ok(()) => {
                            let _ = events.send(E::Quit);
                        }
                        Err(error) => {
                            let _ = events.send(E::Error {
                                message: error.to_string(),
                                recovery_required: false,
                            });
                        }
                    }
                } else {
                    start_ui_transition(
                        executable.clone(),
                        scope,
                        LifecycleAction::Uninstall,
                        vec![],
                        None,
                        true,
                        bridge.clone(),
                    );
                }
            }
            C::Retry => {
                if let Some(intent) = retry_slot.lock().expect("retry state").clone() {
                    start_ui_transition(
                        executable.clone(),
                        intent.scope,
                        intent.action,
                        intent.components,
                        intent.install_directory.clone(),
                        intent.cleanup_lock,
                        bridge.clone(),
                    );
                }
            }
        }
    }
}

#[cfg(feature = "gui")]
fn request_ui_preview(
    executable: PathBuf,
    scope: SelectedScope,
    selected: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    installed_version: Option<semver::Version>,
    events: &std::sync::mpsc::Sender<zup_ui::UiEvent>,
) -> miette::Result<()> {
    let bundle = zup_windows::EmbeddedBundle::open(&executable)
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    let build = embedded_target_plan(&bundle)?;
    let installer = &build.installer;
    let action = resolve_interactive_action(
        LifecycleAction::Install,
        installed_version.as_ref(),
        &installer.app.version,
    )?;
    let enable = selected.iter().map(ToString::to_string).collect::<Vec<_>>();
    let disabled = installer
        .components
        .iter()
        .filter(|item| !item.required && !selected.contains(&item.id))
        .map(|item| item.id.to_string())
        .collect();
    let request = prepare_embedded_transition_with_cancellation(
        action,
        scope,
        None,
        enable,
        disabled,
        install_directory,
        EmbeddedPreparationMode {
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: false,
        },
    )?;
    let mut preview = zup_presentation::PlanPreview::from_transaction_plan(
        &request.request.transaction_plan,
        scope,
    )
    .with_declared_prerequisites(&installer.prerequisites);
    preview.application = installer.app.name.to_string();
    preview.version = installer.app.version.to_string();
    preview.install_directory = request
        .request
        .transaction_plan
        .install_directory
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    events
        .send(zup_ui::UiEvent::PlanReady(preview))
        .map_err(|error| miette::miette!("preview channel: {error}"))?;
    Ok(())
}

#[cfg(feature = "gui")]
fn session_log_path() -> PathBuf {
    std::env::temp_dir().join("zup-ui.log")
}

#[cfg(feature = "gui")]
fn open_log_path(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

#[cfg(feature = "gui")]
fn diagnostic_summary(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "zup diagnostic\nlog: {path}\nNo environment variables or secrets are included."
        ),
        None => "zup diagnostic\nNo session log is available yet.".into(),
    }
}

#[cfg(feature = "gui")]
fn copy_to_clipboard(value: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("clip").arg(value).output();
    }
    #[cfg(not(windows))]
    {
        let _ = value;
    }
}

#[cfg(feature = "gui")]
fn is_maintenance_executable(executable: &Path) -> bool {
    executable
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\maintenance\\")
}

#[cfg(feature = "gui")]
#[derive(Clone)]
struct RetryIntent {
    scope: SelectedScope,
    action: LifecycleAction,
    components: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    cleanup_lock: bool,
}

#[cfg(feature = "gui")]
#[derive(Clone)]
struct UiOperationBridge {
    cancel_slot: std::sync::Arc<std::sync::Mutex<Option<zup_runtime::CancellationHandle>>>,
    retry_slot: std::sync::Arc<std::sync::Mutex<Option<RetryIntent>>>,
    log_slot: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
}

#[cfg(feature = "gui")]
fn start_ui_transition(
    executable: PathBuf,
    scope: SelectedScope,
    action: LifecycleAction,
    selected: Vec<ComponentId>,
    install_directory: Option<PathBuf>,
    cleanup_lock: bool,
    bridge: UiOperationBridge,
) {
    let cancel = zup_runtime::CancellationHandle::new();
    *bridge.cancel_slot.lock().expect("cancel state") = Some(cancel.clone());
    *bridge.retry_slot.lock().expect("retry state") = Some(RetryIntent {
        scope,
        action,
        components: selected.clone(),
        install_directory: install_directory.clone(),
        cleanup_lock,
    });
    let events = bridge.events.clone();
    std::thread::spawn(move || {
        let _ = events.send(zup_ui::UiEvent::Progress {
            completed: 0,
            total: 0,
            action: "Preparing…".into(),
        });
        let enable = selected.iter().map(ToString::to_string).collect::<Vec<_>>();
        let disabled = zup_windows::EmbeddedBundle::open(&executable)
            .ok()
            .and_then(|bundle| embedded_target_plan(&bundle).ok())
            .map(|build| {
                build
                    .installer
                    .components
                    .iter()
                    .filter(|item| !item.required && !selected.contains(&item.id))
                    .map(|item| item.id.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let request = prepare_embedded_transition_with_cancellation(
            action,
            scope,
            None,
            enable,
            disabled,
            install_directory,
            EmbeddedPreparationMode {
                cancellation: &RuntimeCancellationQuery(&cancel),
                acquire_prerequisites: true,
            },
        );
        let prepared = match request {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required: false,
                });
                *bridge.cancel_slot.lock().expect("cancel state") = None;
                return;
            }
        };
        let request = prepared.request.clone();
        let drifted = request
            .transaction_plan
            .retired_keys
            .iter()
            .map(|key| format!("{key:?}"))
            .collect::<Vec<_>>();
        let app_id = request.app_id.clone();
        let state_root = request.state_root.clone();
        let (runtime_events, _) = tokio::sync::broadcast::channel(256);
        let mut rx = runtime_events.subscribe();
        let event_tx = events.clone();
        let log_slot = bridge.log_slot.clone();
        let pump = std::thread::spawn(move || {
            loop {
                let event = match rx.blocking_recv() {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if let zup_runtime::RuntimeEvent::LogPath { path } = &event {
                    *log_slot.lock().expect("log state") = Some(path.clone());
                }
                let terminal = is_terminal_runtime_event(&event);
                let _ = event_tx.send(zup_ui::UiEvent::Runtime(event));
                if terminal {
                    break;
                }
            }
        });
        let recovery = request.recovery_id.is_some();
        match execute_with_control(prepared, cancel, runtime_events) {
            Ok(outcome) => {
                if matches!(&outcome, InstallOutcome::Failed(message) if message == "blocked by running applications")
                {
                    *bridge.cancel_slot.lock().expect("cancel state") = None;
                    let _ = pump.join();
                    return;
                }
                if matches!(action, LifecycleAction::Repair { .. })
                    && outcome == InstallOutcome::Committed
                {
                    let _ = events.send(zup_ui::UiEvent::RepairFinished {
                        drifted_resources: drifted,
                    });
                }
                if cleanup_lock && outcome == InstallOutcome::Committed {
                    let _ = remove_uninstall_lock(&state_root, &app_id, scope);
                    let _ = zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope);
                }
                let _ = events.send(zup_ui::UiEvent::OperationFinished(outcome));
            }
            Err(error) => {
                let recovery_required = recovery || error.to_string().contains("recovery required");
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required,
                });
            }
        }
        let _ = pump.join();
        *bridge.cancel_slot.lock().expect("cancel state") = None;
    });
}

#[cfg(feature = "gui")]
fn installed_components(
    executable: &Path,
    scope: SelectedScope,
) -> miette::Result<Vec<ComponentId>> {
    let bundle = zup_windows::EmbeddedBundle::open(executable)
        .map_err(|e| miette::miette!("package: {e}"))?;
    let build = embedded_target_plan(&bundle)?;
    let state = choose_state_root(None, scope)?;
    let ledger = zup_windows::InstallLedgerStore::new(&state)
        .load(&build.installer.app.id, scope)
        .map_err(|e| miette::miette!("ledger: {e}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    Ok(ledger.selected_components)
}

#[cfg(feature = "gui")]
fn update_from_ui(
    executable: &Path,
    scope: SelectedScope,
    mut status: impl FnMut(&str),
) -> Result<Option<(String, String)>, String> {
    let bundle = zup_windows::EmbeddedBundle::open(executable).map_err(|e| e.to_string())?;
    let build = embedded_target_plan(&bundle).map_err(|error| error.to_string())?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| "Updates are not configured for this application".to_owned())?;
    let state = choose_state_root(None, scope).map_err(|e| e.to_string())?;
    let ledger = zup_windows::InstallLedgerStore::new(&state)
        .load(&installer.app.id, scope)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Installation not found".to_owned())?;
    let update_root = if scope == SelectedScope::Machine {
        default_state_root(SelectedScope::User).map_err(|e| e.to_string())?
    } else {
        state.clone()
    };
    let client = zup_update::Client::new(config, installer.app.id.as_str(), &update_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    status("Checking for updates…");
    match runtime
        .block_on(client.check(&ledger.version))
        .map_err(|e| e.to_string())?
    {
        zup_update::CheckResult::UpToDate { current: _ } => {
            status("Up to date");
            Ok(None)
        }
        zup_update::CheckResult::UpdateAvailable {
            current,
            available,
            target,
        } => {
            let destination = update_root
                .join("updates")
                .join("downloads")
                .join(format!("Setup-{}.exe", uuid::Uuid::now_v7()));
            status("Downloading update…");
            runtime
                .block_on(client.download(&target, &destination))
                .map_err(|e| e.to_string())?;
            status("Verifying update…");
            validate_downloaded_update(
                &destination,
                &installer.app.id,
                &available,
                &installer.target,
                installer.frontend,
                scope,
            )
            .map_err(|error| error.to_string())?;
            status("Ready to install");
            std::process::Command::new(destination)
                .arg("upgrade")
                .arg("--scope")
                .arg(scope.to_string())
                .arg("--state-root")
                .arg(&state)
                .arg("--ui")
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(Some((current.to_string(), available.to_string())))
        }
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

#[cfg(all(test, feature = "build"))]
mod tests {
    use std::cell::Cell;

    #[cfg(any(feature = "gui", feature = "console"))]
    use semver::Version;
    use tempfile::TempDir;
    use zup_plan::{
        CancellationQuery, PluginExecutor, PluginFailure, PluginPlanningContext,
        PluginResourceProposal,
    };

    use super::*;

    struct FakeExecutor {
        target: zup_core::TargetTriple,
    }

    impl PluginExecutor for FakeExecutor {
        fn target(&self) -> &zup_core::TargetTriple {
            &self.target
        }

        fn plan(
            &mut self,
            _binding: &zup_core::PluginBinding,
            _context: &PluginPlanningContext,
            _cancellation: &dyn CancellationQuery,
        ) -> Result<PluginResourceProposal, PluginFailure> {
            unreachable!()
        }
    }

    fn build_with_plugin(root: &TempDir) -> zup_build::TargetBuildPlan {
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::create_dir_all(root.path().join("plugins")).unwrap();
        std::fs::write(root.path().join("plugins/helper.wasm"), b"plugin").unwrap();
        let source = r#"
schema = 1
[app]
id = "com.example.embedded-plugin"
name = "Embedded Plugin"
version = "1.0.0"
[build]
[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/EmbeddedPlugin"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#;
        let manifest = zup_manifest::parse(source).unwrap();
        let overrides = TargetOverrides::default();
        let config = zup_manifest::select_targets(&manifest, &["default"], &overrides)
            .unwrap()
            .remove(0);
        let installer = zup_manifest::compile(&manifest, &config, &overrides).unwrap();
        let mut build = zup_build::materialize(
            &root.path().join("zup.toml"),
            &manifest,
            vec![(config, installer)],
        )
        .unwrap();
        build.targets.pop().unwrap()
    }

    #[cfg(any(feature = "gui", feature = "console"))]
    #[test]
    fn interactive_install_resolves_existing_versions() {
        let package = Version::parse("2.0.0").unwrap();
        assert_eq!(
            resolve_interactive_action(LifecycleAction::Install, None, &package).unwrap(),
            LifecycleAction::Install
        );
        assert_eq!(
            resolve_interactive_action(
                LifecycleAction::Install,
                Some(&Version::parse("1.0.0").unwrap()),
                &package,
            )
            .unwrap(),
            LifecycleAction::Upgrade
        );
        assert_eq!(
            resolve_interactive_action(LifecycleAction::Install, Some(&package), &package,)
                .unwrap(),
            LifecycleAction::Modify
        );
        assert!(
            resolve_interactive_action(
                LifecycleAction::Install,
                Some(&Version::parse("3.0.0").unwrap()),
                &package,
            )
            .is_err()
        );
    }

    #[test]
    fn uninstall_does_not_load_the_plugin_executor() {
        let root = TempDir::new().unwrap();
        let build = build_with_plugin(&root);
        let mut ledger = zup_exec::InstallLedger::new(
            build.installer.app.id.clone(),
            build.installer.target.clone(),
            SelectedScope::User,
        );
        ledger.version = build.installer.app.version.clone();
        let loads = Cell::new(0);
        let result = prepare_embedded_request(
            EmbeddedPreparation {
                action: LifecycleAction::Uninstall,
                scope: SelectedScope::User,
                build: &build,
                prior: Some(ledger),
                state_root: root.path().join("state"),
                payload_root: root.path().to_path_buf(),
                enable: Vec::new(),
                disable: Vec::new(),
                install_directory: None,
            },
            None,
            true,
            &zup_plan::NeverCancelled,
            || {
                loads.set(loads.get() + 1);
                Ok(FakeExecutor {
                    target: build.installer.target.clone(),
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(loads.get(), 0);
    }

    #[test]
    fn ui_cancellation_is_forwarded_before_lifecycle_planning() {
        let root = TempDir::new().unwrap();
        let build = build_with_plugin(&root);
        let cancel = zup_runtime::CancellationHandle::new();
        cancel.cancel();
        let result = prepare_embedded_request(
            EmbeddedPreparation {
                action: LifecycleAction::Install,
                scope: SelectedScope::User,
                build: &build,
                prior: None,
                state_root: root.path().join("state"),
                payload_root: root.path().to_path_buf(),
                enable: Vec::new(),
                disable: Vec::new(),
                install_directory: None,
            },
            None,
            true,
            &RuntimeCancellationQuery(&cancel),
            || {
                Ok(FakeExecutor {
                    target: build.installer.target.clone(),
                })
            },
        );
        let error = match result {
            Ok(_) => panic!("cancelled planning unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("planning was cancelled"));
    }
}
