//! Read-only build readiness for `zup doctor`.
//!
//! `zup doctor` answers one question: would `zup build` succeed right now? It
//! reuses the same manifest, target, plugin, lowering, and build-input paths as
//! `zup build`, then reports every check it cannot pass.
//!
//! The command is read-only. It never writes an installer artifact, downloads a
//! tool, or changes machine state: plugin sources are hashed and AOT-compiled
//! in memory, and output directories are probed for write access without
//! creating anything. Every failure across every selected target is collected
//! and reported, and the process exits nonzero when a check fails.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum, ValueHint};
use serde::Serialize;
use zup_core::{ResolvedTargetConfig, Sha256Digest, hash_reader};
use zup_plugin_contract::{PluginEngine, WASMTIME_VERSION};

use crate::build_inputs::{self, BackendSupport, BuildInputs};

/// Version of the `zup doctor` report shape.
///
/// Bump this whenever a field is added, removed, or retyped, so a consumer can
/// tell two report documents apart before it reads them.
pub const REPORT_VERSION: u32 = 1;

/// Report build readiness without writing anything.
#[derive(Debug, Args)]
pub struct DoctorCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    /// Runtime template for each selected target; discovered for a single target.
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub runtime: Vec<PathBuf>,
    /// Installer output for each selected target; derived when omitted.
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Vec<PathBuf>,
    /// Build source directory for each selected target, relative to the project.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    /// Default install directory to resolve for each selected target.
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    /// Frontend to resolve for each selected target.
    #[arg(long, value_enum)]
    pub frontend: Option<crate::FrontendArg>,
    /// Target profile name or canonical target triple. Repeatable; empty selects all.
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    #[arg(long, value_enum, default_value = "human")]
    pub format: FormatArg,
}

impl Default for DoctorCommand {
    /// A human-readable readiness report for the default manifest.
    fn default() -> Self {
        Self {
            manifest: PathBuf::from(crate::DEFAULT_MANIFEST),
            runtime: Vec::new(),
            output: Vec::new(),
            source: Vec::new(),
            install_directory: Vec::new(),
            frontend: None,
            target: Vec::new(),
            format: FormatArg::Human,
        }
    }
}

/// Machine-readable shape of a readiness report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    Human,
    Json,
}

impl FormatArg {
    const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

impl From<FormatArg> for zup_presentation::OutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Human => Self::Human,
            FormatArg::Json => Self::Json,
        }
    }
}

/// Result of one readiness check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check failed; `zup build` would fail or refuse this target.
    Fail,
    /// The check does not apply, or an earlier failure made it meaningless.
    Skip,
}

/// What one readiness check examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// The canonical target triple the profile resolves to.
    CanonicalTarget,
    /// Compilation of the profile into installer IR.
    ManifestCompile,
    /// The source root and the payload it materializes.
    SourcePayload,
    /// Plugin engine construction and AOT compilation.
    PluginEngine,
    /// The embedded trusted update root.
    UpdateRoot,
    /// The frontend this profile resolves to.
    Frontend,
    /// Runtime template existence and template identity.
    RuntimeTemplate,
    /// The runtime PE machine against the canonical target.
    RuntimeTarget,
    /// The runtime PE subsystem against the resolved frontend.
    RuntimeSubsystem,
    /// Backend support for the target on this build host.
    BuildBackend,
    /// Windows target lowering for every install scope.
    TargetLowering,
    /// Whether the output parent directory can be written.
    OutputParent,
}

impl CheckKind {
    const fn label(self) -> &'static str {
        match self {
            Self::CanonicalTarget => "target",
            Self::ManifestCompile => "compile",
            Self::SourcePayload => "payload",
            Self::PluginEngine => "plugins",
            Self::UpdateRoot => "updates",
            Self::Frontend => "frontend",
            Self::RuntimeTemplate => "runtime",
            Self::RuntimeTarget => "runtime target",
            Self::RuntimeSubsystem => "subsystem",
            Self::BuildBackend => "backend",
            Self::TargetLowering => "lowering",
            Self::OutputParent => "output",
        }
    }
}

/// One readiness check for one target profile.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub profile: String,
    pub target: String,
    pub kind: CheckKind,
    pub status: CheckStatus,
    pub message: String,
    /// The path the check examined, when it examined one.
    pub path: Option<String>,
}

/// Readiness of one selected target profile.
#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub profile: String,
    pub target: String,
    pub status: CheckStatus,
    pub checks: Vec<Check>,
}

impl TargetReport {
    const fn is_ready(&self) -> bool {
        matches!(self.status, CheckStatus::Pass)
    }
}

/// A complete readiness report for the selected targets.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: u32,
    pub manifest: String,
    /// The canonical target triple of the build host.
    pub host: String,
    pub status: CheckStatus,
    pub targets: Vec<TargetReport>,
}

impl DoctorReport {
    /// The number of checks that failed.
    pub fn failures(&self) -> usize {
        self.targets
            .iter()
            .flat_map(|target| &target.checks)
            .filter(|check| check.status == CheckStatus::Fail)
            .count()
    }

    /// Whether every selected target is ready to build.
    pub fn is_ready(&self) -> bool {
        self.targets.iter().all(TargetReport::is_ready)
    }

    /// Render the report for a terminal.
    pub fn human(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "manifest  {}", self.manifest);
        let _ = writeln!(text, "host      {}", self.host);
        for target in &self.targets {
            let _ = writeln!(
                text,
                "\n{} {} · {}",
                status_glyph(target.status),
                target.profile,
                target.target
            );
            for check in &target.checks {
                let _ = writeln!(
                    text,
                    "  {} {:<14} {}",
                    status_glyph(check.status),
                    check.kind.label(),
                    check.message
                );
                if check.status == CheckStatus::Fail
                    && let Some(path) = &check.path
                {
                    let _ = writeln!(text, "    {path}");
                }
            }
        }
        let _ = writeln!(
            text,
            "\n{}",
            if self.is_ready() {
                format!("ready: all {} target(s) can be built", self.targets.len())
            } else {
                format!(
                    "not ready: {} failing check(s) across {} target(s)",
                    self.failures(),
                    self.targets.len()
                )
            }
        );
        text
    }
}

const fn status_glyph(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "✓",
        CheckStatus::Fail => "✗",
        CheckStatus::Skip => "-",
    }
}

/// Render a build-machine path for a report, without the Windows verbatim prefix.
fn display(path: &Path) -> String {
    crate::target_path_text(path)
}

/// Run `zup doctor`: print the report, then fail when a required check failed.
pub fn run(args: DoctorCommand) -> miette::Result<()> {
    let report = inspect(&args)?;
    if args.format.is_json() {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|error| miette::miette!("report: {error}"))?;
        println!("{json}");
    } else {
        print!("{}", report.human());
    }
    if report.is_ready() {
        return Ok(());
    }
    // The report already described the failure; keep stdout free of a second envelope.
    crate::OUTPUT_FAILURE_EMITTED.store(true, std::sync::atomic::Ordering::SeqCst);
    Err(miette::miette!(
        "build readiness: {} check(s) failed across {} target(s)",
        report.failures(),
        report.targets.len()
    ))
}

/// Checks that need a materialized build plan for this target.
///
/// The update root and the frontend are resolved from the manifest and the
/// target profile alone, so they are evaluated even without a plan.
const PLAN_KINDS: [CheckKind; 4] = [
    CheckKind::ManifestCompile,
    CheckKind::SourcePayload,
    CheckKind::PluginEngine,
    CheckKind::TargetLowering,
];

/// The check that owns a plan failure, so it is reported once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanOwner {
    Compile,
    Payload,
    Plugins,
    UpdateRoot,
}

impl PlanOwner {
    const fn kind(self) -> CheckKind {
        match self {
            Self::Compile => CheckKind::ManifestCompile,
            Self::Payload => CheckKind::SourcePayload,
            Self::Plugins => CheckKind::PluginEngine,
            Self::UpdateRoot => CheckKind::UpdateRoot,
        }
    }
}

/// The plan a target produced, or the check that owns the failure.
enum PlanOutcome {
    Ready(Box<zup_build::BuildPlan>),
    Failed(PlanOwner, String),
}

struct Inspection<'a> {
    manifest: &'a zup_manifest::Manifest,
    manifest_path: &'a Path,
    manifest_name: &'a str,
    source: &'a str,
    project: &'a Path,
    overrides: &'a zup_manifest::TargetOverrideSet,
    inputs: &'a BuildInputs,
}

fn inspect(args: &DoctorCommand) -> miette::Result<DoctorReport> {
    let overrides = crate::TargetOverrideArgs {
        source: args.source.clone(),
        install_directory: args.install_directory.clone(),
        frontend: args.frontend.map(zup_core::Frontend::from),
    };
    let selected = crate::select_project(&args.manifest, &args.target, &overrides, false)?;
    let manifest_path = selected.manifest_path.clone();
    let manifest_name = selected.manifest_name.clone();
    let inputs = build_inputs::resolve_build_inputs(
        build_inputs::InputMode::Preflight,
        build_inputs::Overwrite::Refuse,
        &args.runtime,
        &args.output,
        &manifest_path,
        &selected.manifest.app,
        &selected.selected_targets,
    )?;
    let project = zup_build::project_root(&manifest_path);
    let inspection = Inspection {
        manifest: &selected.manifest,
        manifest_path: &selected.manifest_path,
        manifest_name: &selected.manifest_name,
        source: &selected.source,
        project: &project,
        overrides: &selected.overrides,
        inputs: &inputs,
    };
    let targets = selected
        .selected_targets
        .iter()
        .enumerate()
        .map(|(index, config)| inspection.check_target(index, config))
        .collect::<Vec<_>>();
    Ok(DoctorReport {
        version: REPORT_VERSION,
        manifest: manifest_name,
        host: zup_plugin_contract::HOST_TARGET.to_owned(),
        status: if targets.iter().all(TargetReport::is_ready) {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        targets,
    })
}

impl Inspection<'_> {
    fn check_target(&self, index: usize, config: &ResolvedTargetConfig) -> TargetReport {
        let mut report = TargetChecks {
            inputs: self.inputs,
            manifest: self.manifest,
            manifest_path: self.manifest_path,
            project: self.project,
            index,
            config,
            checks: Vec::with_capacity(14),
        };

        let target = &config.target;
        report.pass(
            CheckKind::CanonicalTarget,
            format!("canonical target is `{target}`"),
            None,
        );

        let source_root = zup_build::resolve_source_root(self.project, &config.source.directory);
        let update_root = zup_build::resolve_update_root(self.project, self.manifest);
        let plan = self.plan(config, &source_root, &update_root);
        if let PlanOutcome::Ready(build) = &plan {
            report.compiled(build);
            report.payload(build, &source_root);
            report.plugins(build);
        } else {
            report.plan_unavailable(&plan);
        }
        report.update_root(&update_root);
        report.frontend(&plan);
        report.input_problems(build_inputs::InputSubject::Runtime);
        report.runtime();
        report.backend();
        if let PlanOutcome::Ready(build) = &plan {
            report.lowering(build);
        }
        report.input_problems(build_inputs::InputSubject::Output);
        report.output();
        report.finish()
    }

    /// Compile and materialize one target, keeping the failure typed.
    fn plan(
        &self,
        config: &ResolvedTargetConfig,
        source_root: &Result<PathBuf, zup_build::BuildError>,
        update_root: &Result<Option<zup_build::ResolvedUpdateRoot>, zup_build::BuildError>,
    ) -> PlanOutcome {
        let overrides = self.overrides.get(&config.profile);
        let installer = match zup_manifest::compile(self.manifest, config, overrides) {
            Ok(installer) => installer,
            Err(error) => {
                let report =
                    miette::Report::new(error.with_source_named(self.source, self.manifest_name));
                return PlanOutcome::Failed(PlanOwner::Compile, report.to_string());
            }
        };
        let selection = vec![(config.clone(), installer)];
        match zup_build::materialize_with_policy(
            self.manifest_path,
            self.manifest,
            selection,
            &zup_windows::WindowsSourceFilePolicy,
        ) {
            Ok(build) => PlanOutcome::Ready(Box::new(build)),
            Err(error) => {
                // Report the cause once, under the check that owns it.
                let owner = if source_root.is_err() {
                    PlanOwner::Payload
                } else if update_root.is_err() {
                    PlanOwner::UpdateRoot
                } else if is_plugin_error(&error) {
                    PlanOwner::Plugins
                } else {
                    PlanOwner::Payload
                };
                PlanOutcome::Failed(owner, error.to_string())
            }
        }
    }
}

/// Whether a materialization failure is about a declared plugin source.
fn is_plugin_error(error: &zup_build::BuildError) -> bool {
    let error = match error {
        zup_build::BuildError::Target { source, .. } => source.as_ref(),
        other => other,
    };
    matches!(
        error,
        zup_build::BuildError::PluginSourceMissing { .. }
            | zup_build::BuildError::PluginSourceEscapesProject { .. }
            | zup_build::BuildError::PluginSourceSymlink { .. }
            | zup_build::BuildError::PluginSourceNotRegular { .. }
            | zup_build::BuildError::PluginSourceTooLarge { .. }
            | zup_build::BuildError::PluginSourceChanged { .. }
            | zup_build::BuildError::PluginSourceMismatch { .. }
            | zup_build::BuildError::TooManyPluginDeclarations { .. }
    )
}

/// Ordered check rows for one target profile.
struct TargetChecks<'a> {
    inputs: &'a BuildInputs,
    manifest: &'a zup_manifest::Manifest,
    manifest_path: &'a Path,
    project: &'a Path,
    index: usize,
    config: &'a ResolvedTargetConfig,
    checks: Vec<Check>,
}

impl TargetChecks<'_> {
    fn finish(self) -> TargetReport {
        let status = if self
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
        {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        };
        TargetReport {
            profile: self.config.profile.to_string(),
            target: self.config.target.to_string(),
            status,
            checks: self.checks,
        }
    }

    fn pass(&mut self, kind: CheckKind, message: impl Into<String>, path: Option<&Path>) {
        self.record(kind, CheckStatus::Pass, message, path);
    }

    fn fail(&mut self, kind: CheckKind, message: impl Into<String>, path: Option<&Path>) {
        self.record(kind, CheckStatus::Fail, message, path);
    }

    fn skip(&mut self, kind: CheckKind, message: impl Into<String>, path: Option<&Path>) {
        self.record(kind, CheckStatus::Skip, message, path);
    }

    fn record(
        &mut self,
        kind: CheckKind,
        status: CheckStatus,
        message: impl Into<String>,
        path: Option<&Path>,
    ) {
        self.checks.push(Check {
            profile: self.config.profile.to_string(),
            target: self.config.target.to_string(),
            kind,
            status,
            message: message.into(),
            path: path.map(display),
        });
    }

    /// One row per shared input problem, reported against the input it names.
    fn input_problems(&mut self, subject: build_inputs::InputSubject) {
        for problem in self
            .inputs
            .problems_for(self.index)
            .filter(|problem| problem.subject == subject)
            .cloned()
            .collect::<Vec<_>>()
        {
            let kind = match subject {
                build_inputs::InputSubject::Runtime => CheckKind::RuntimeTemplate,
                build_inputs::InputSubject::Output => CheckKind::OutputParent,
            };
            self.fail(kind, problem.message, problem.path.as_deref());
        }
    }

    fn compiled(&mut self, build: &zup_build::BuildPlan) {
        let installer = &build.targets[0].installer;
        self.pass(
            CheckKind::ManifestCompile,
            format!(
                "installer IR: {} component(s), {} file mapping(s), {} plugin(s)",
                installer.components.len(),
                installer.files.len(),
                installer.plugins.len()
            ),
            Some(self.manifest_path),
        );
    }

    fn payload(
        &mut self,
        build: &zup_build::BuildPlan,
        source_root: &Result<PathBuf, zup_build::BuildError>,
    ) {
        let plan = &build.targets[0];
        let root = source_root.as_ref().ok().cloned().unwrap_or_default();
        match payload_digest(plan) {
            Ok(digest) => self.pass(
                CheckKind::SourcePayload,
                format!(
                    "{} file(s) · {} · payload digest {digest}",
                    plan.files.len(),
                    zup_presentation::format_bytes(plan.total_size)
                ),
                Some(&root),
            ),
            Err(error) => self.fail(
                CheckKind::SourcePayload,
                format!("payload could not be summarized: {error}"),
                Some(&root),
            ),
        }
    }

    fn plugins(&mut self, build: &zup_build::BuildPlan) {
        let plan = &build.targets[0];
        if plan.plugins.is_empty() {
            self.skip(
                CheckKind::PluginEngine,
                "no plugins are declared for this target",
                None,
            );
            return;
        }
        let source = plan.plugins.first().map(|plugin| plugin.source.clone());
        let target = self.config.target.as_str();
        let engine = match PluginEngine::new(target) {
            Ok(engine) => engine,
            Err(error) => {
                self.fail(
                    CheckKind::PluginEngine,
                    format!("plugin engine for `{target}` is unavailable: {error}"),
                    source.as_deref(),
                );
                return;
            }
        };
        // AOT compilation happens in memory; nothing is written or installed.
        match zup_plugin_build::compile_plugins(plan) {
            Ok(artifacts) => self.pass(
                CheckKind::PluginEngine,
                format!(
                    "{} plugin(s) compiled and verified with Wasmtime {WASMTIME_VERSION} · engine {}",
                    artifacts.len(),
                    engine.fingerprint()
                ),
                source.as_deref(),
            ),
            Err(error) => self.fail(
                CheckKind::PluginEngine,
                format!("plugin AOT compilation is not ready: {error}"),
                source.as_deref(),
            ),
        }
    }

    /// Mark every plan-dependent check; only the owning check failed.
    fn plan_unavailable(&mut self, plan: &PlanOutcome) {
        let PlanOutcome::Failed(owner, message) = plan else {
            return;
        };
        for kind in PLAN_KINDS {
            if kind == owner.kind() {
                self.fail(kind, message.clone(), None);
            } else {
                self.skip(
                    kind,
                    format!("not evaluated: the {} check failed", owner.kind().label()),
                    None,
                );
            }
        }
    }

    fn update_root(
        &mut self,
        root: &Result<Option<zup_build::ResolvedUpdateRoot>, zup_build::BuildError>,
    ) {
        let updates = self.manifest.updates.as_ref();
        match root {
            Ok(Some(root)) => {
                let channel = updates
                    .map(|updates| updates.channel.as_str())
                    .unwrap_or("unknown");
                self.pass(
                    CheckKind::UpdateRoot,
                    format!(
                        "trusted update root embedded: {} bytes · sha256 {} · channel {channel}",
                        root.bytes.len(),
                        root.sha256
                    ),
                    Some(&root.path),
                );
            }
            Ok(None) => self.skip(
                CheckKind::UpdateRoot,
                "no [updates] section; no trusted update root is embedded",
                None,
            ),
            Err(error) => {
                let path = updates.map(|updates| self.project.join(&updates.root));
                self.fail(CheckKind::UpdateRoot, error.to_string(), path.as_deref());
            }
        }
    }

    fn frontend(&mut self, plan: &PlanOutcome) {
        let frontend = self.config.frontend;
        if let PlanOutcome::Ready(build) = plan {
            let compiled = build.targets[0].installer.frontend;
            if compiled != frontend {
                self.fail(
                    CheckKind::Frontend,
                    format!(
                        "profile resolves to `{frontend}` but the installer IR is `{compiled}`"
                    ),
                    Some(self.manifest_path),
                );
                return;
            }
        }
        self.pass(
            CheckKind::Frontend,
            format!("resolved frontend is `{frontend}`"),
            Some(self.manifest_path),
        );
    }

    /// Template existence and identity for the resolved runtime.
    fn runtime_template(&mut self, runtime: &Path) {
        let frontend = self.config.frontend;
        if !runtime.is_file() {
            self.fail(
                CheckKind::RuntimeTemplate,
                format!("runtime template `{}` is not a file", display(runtime)),
                Some(runtime),
            );
            return;
        }
        match build_inputs::validate_runtime_template(runtime, frontend) {
            Ok(()) => {
                let discovered = if self.inputs.runtimes[self.index].discovered {
                    " (discovered next to this executable)"
                } else {
                    ""
                };
                self.pass(
                    CheckKind::RuntimeTemplate,
                    format!("runtime template matches the {frontend} frontend{discovered}"),
                    Some(runtime),
                );
            }
            Err(error) => self.fail(CheckKind::RuntimeTemplate, error.to_string(), Some(runtime)),
        }
    }

    /// The runtime PE machine against the canonical target.
    fn runtime_target(&mut self, runtime: &Path) {
        match zup_windows::read_pe_target(runtime) {
            Ok(found) if found != self.config.target => self.fail(
                CheckKind::RuntimeTarget,
                format!(
                    "runtime target `{found}` does not match target `{}`",
                    self.config.target
                ),
                Some(runtime),
            ),
            Ok(found) => self.pass(
                CheckKind::RuntimeTarget,
                format!("runtime target matches `{found}`"),
                Some(runtime),
            ),
            Err(error) => self.fail(
                CheckKind::RuntimeTarget,
                format!("runtime target could not be read: {error}"),
                Some(runtime),
            ),
        }
    }

    /// The runtime PE subsystem against the resolved frontend.
    fn runtime_subsystem(&mut self, runtime: &Path) {
        let frontend = self.config.frontend;
        let expected = build_inputs::expected_subsystem(frontend);
        match zup_windows::read_pe_subsystem(runtime) {
            Ok(found) if found != expected => self.fail(
                CheckKind::RuntimeSubsystem,
                format!(
                    "runtime subsystem is {:?} but the {frontend} frontend requires {expected:?}",
                    found
                ),
                Some(runtime),
            ),
            Ok(found) => self.pass(
                CheckKind::RuntimeSubsystem,
                format!("runtime subsystem is {found:?} for the {frontend} frontend"),
                Some(runtime),
            ),
            Err(error) => self.fail(
                CheckKind::RuntimeSubsystem,
                format!("runtime subsystem could not be read: {error}"),
                Some(runtime),
            ),
        }
    }

    /// The runtime checks, or a skip for each when no runtime was resolved.
    fn runtime(&mut self) {
        let Some(runtime) = self.inputs.runtimes[self.index].path.clone() else {
            for kind in [
                CheckKind::RuntimeTemplate,
                CheckKind::RuntimeTarget,
                CheckKind::RuntimeSubsystem,
            ] {
                self.skip(kind, "not evaluated: no runtime was resolved", None);
            }
            return;
        };
        self.runtime_template(&runtime);
        self.runtime_target(&runtime);
        self.runtime_subsystem(&runtime);
    }

    fn backend(&mut self) {
        let target = &self.config.target;
        match build_inputs::backend_support(target) {
            BackendSupport::Ready => self.pass(
                CheckKind::BuildBackend,
                format!(
                    "the implemented Windows backend is ready on this build host for `{target}`"
                ),
                None,
            ),
            other => self.fail(
                CheckKind::BuildBackend,
                other
                    .reason(target)
                    .expect("only a ready backend has no reason"),
                None,
            ),
        }
    }

    fn lowering(&mut self, build: &zup_build::BuildPlan) {
        let support = build_inputs::backend_support(&self.config.target);
        if support != BackendSupport::Ready {
            self.skip(
                CheckKind::TargetLowering,
                format!(
                    "not evaluated: {}",
                    support
                        .reason(&self.config.target)
                        .expect("an unavailable backend always has a reason")
                ),
                None,
            );
            return;
        }
        match build_inputs::check_target_lowering(build, self.config) {
            Ok(scopes) => {
                let scopes = scopes
                    .iter()
                    .map(|scope| scope.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.pass(
                    CheckKind::TargetLowering,
                    format!("Windows lowering resolved for scope(s) {scopes}"),
                    None,
                );
            }
            Err(error) => self.fail(CheckKind::TargetLowering, error.to_string(), None),
        }
    }

    fn output(&mut self) {
        let slot = &self.inputs.outputs[self.index];
        let output = slot.path.clone();
        let derived = if slot.derived {
            " (derived output name)"
        } else {
            ""
        };
        let absolute = build_inputs::absolute_path(&output);
        let Some(parent) = absolute.parent().map(Path::to_path_buf) else {
            self.fail(
                CheckKind::OutputParent,
                format!("output `{}` has no parent directory", display(&output)),
                Some(&output),
            );
            return;
        };
        if !parent.exists() {
            self.fail(
                CheckKind::OutputParent,
                format!(
                    "output parent `{}` does not exist; create it or choose an --output in an existing directory",
                    display(&parent)
                ),
                Some(&output),
            );
            return;
        }
        if !parent.is_dir() {
            self.fail(
                CheckKind::OutputParent,
                format!("output parent `{}` is not a directory", display(&parent)),
                Some(&output),
            );
            return;
        }
        match directory_accepts_writes(&parent) {
            Ok(true) => self.pass(
                CheckKind::OutputParent,
                format!(
                    "output parent `{}` accepts writes{derived}; `zup build` writes `{}`",
                    display(&parent),
                    display(&output)
                ),
                Some(&output),
            ),
            Ok(false) => self.fail(
                CheckKind::OutputParent,
                format!("output parent `{}` is not writable", display(&parent)),
                Some(&output),
            ),
            Err(error) => self.fail(
                CheckKind::OutputParent,
                format!(
                    "output parent `{}` could not be probed: {error}",
                    display(&parent)
                ),
                Some(&output),
            ),
        }
    }
}

/// A stable digest over the ordered payload inventory of one target.
///
/// This summarizes what the payload contains without rehashing the sources.
fn payload_digest(plan: &zup_build::TargetBuildPlan) -> io::Result<Sha256Digest> {
    let mut summary = String::with_capacity(plan.files.len() * 96);
    for file in &plan.files {
        let _ = writeln!(
            summary,
            "{}\t{}\t{}",
            file.destination,
            file.size,
            file.sha256.to_hex()
        );
    }
    hash_reader(summary.as_bytes()).map(|(_, digest)| digest)
}

/// Whether a directory can be written, without writing anything.
///
/// On Windows the ACL is probed by opening the directory for write access with
/// backup semantics, which needs no privilege and creates nothing.
#[cfg(windows)]
fn directory_accepts_writes(directory: &Path) -> io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether a directory can be written, read from its permission bits.
#[cfg(unix)]
fn directory_accepts_writes(directory: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    Ok(std::fs::metadata(directory)?.permissions().mode() & 0o222 != 0)
}

/// Whether a directory can be written, read from its read-only flag.
#[cfg(not(any(windows, unix)))]
fn directory_accepts_writes(directory: &Path) -> io::Result<bool> {
    Ok(!std::fs::metadata(directory)?.permissions().readonly())
}
