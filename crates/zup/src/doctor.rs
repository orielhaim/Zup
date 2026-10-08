//! The command is read-only. It never writes an installer artifact, downloads a

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use clap::{Args, ValueHint};
use zup_automation::{AutomationResult, Details, LogLevel};
use zup_core::{ResolvedTargetConfig, Sha256Digest, hash_reader};
use zup_plugin_contract::{PluginEngine, WASMTIME_VERSION};

use crate::build_inputs::{self, BackendSupport, BuildInputs};
use crate::cli::OutputArg;
use crate::failure::Reporter;
#[cfg(target_os = "linux")]
use zup_linux::SystemdManager as _;

pub const TOOLCHAIN_HINT: &str = "Build the zup toolchain for this version and stage it beside \
                             `zup`, or point zup at one with `zup --toolchain <dir>`";

#[derive(Debug, Args)]
pub struct DoctorCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    pub manifest: PathBuf,
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub runtime: Vec<PathBuf>,
    #[arg(long, value_hint = ValueHint::FilePath)]
    pub output: Vec<PathBuf>,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Vec<PathBuf>,
    #[arg(long, alias = "install-dir", value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub install_directory: Vec<PathBuf>,
    #[arg(long, value_enum)]
    pub frontend: Option<crate::FrontendArg>,
    #[arg(long, value_name = "PROFILE_OR_TARGET")]
    pub target: Vec<String>,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

impl Default for DoctorCommand {
    fn default() -> Self {
        Self {
            manifest: PathBuf::from(crate::DEFAULT_MANIFEST),
            runtime: Vec::new(),
            output: Vec::new(),
            source: Vec::new(),
            install_directory: Vec::new(),
            frontend: None,
            target: Vec::new(),
            format: OutputArg::Human,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    CanonicalTarget,
    ManifestCompile,
    SourcePayload,
    PluginEngine,
    UpdateRoot,
    Frontend,
    RuntimeTemplate,
    BuildBackend,
    TargetLowering,
    OutputParent,
    Elevation,
    ServiceRuntime,
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
            Self::BuildBackend => "backend",
            Self::TargetLowering => "lowering",
            Self::OutputParent => "output",
            Self::Elevation => "elevation",
            Self::ServiceRuntime => "services",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub profile: String,
    pub target: String,
    pub kind: CheckKind,
    pub status: CheckStatus,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub manifest: String,
    pub host: String,
    pub status: CheckStatus,
    pub targets: Vec<TargetReport>,
}

impl DoctorReport {
    pub fn failures(&self) -> usize {
        self.targets
            .iter()
            .flat_map(|target| &target.checks)
            .filter(|check| check.status == CheckStatus::Fail)
            .count()
    }

    pub fn is_ready(&self) -> bool {
        self.targets.iter().all(TargetReport::is_ready)
    }

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

fn display(path: &Path) -> String {
    crate::plain_path(path)
}

pub fn run(
    args: DoctorCommand,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let report = inspect(&args, &crate::resolver(toolchain_root)?)?;
    reporter.log(LogLevel::Info, report.human());
    let details = crate::automation::doctor(&report);
    let mut result = AutomationResult::new(zup_automation::OPERATION_DOCTOR)
        .with_details(Details::Doctor(details.clone()))
        .with_summary(format!(
            "{}: {}",
            if report.is_ready() {
                "ready"
            } else {
                "not ready"
            },
            if report.is_ready() {
                format!("all {} target(s) can be built", report.targets.len())
            } else {
                format!(
                    "{} failing check(s) across {} target(s)",
                    report.failures(),
                    report.targets.len()
                )
            }
        ));
    if !report.is_ready() {
        result = result
            .failed()
            .with_diagnostic(crate::automation::doctor_diagnostic(&report));
    }
    Ok(result)
}

const PLAN_KINDS: [CheckKind; 4] = [
    CheckKind::ManifestCompile,
    CheckKind::SourcePayload,
    CheckKind::PluginEngine,
    CheckKind::TargetLowering,
];

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

fn inspect(
    args: &DoctorCommand,
    resolver: &crate::ToolchainResolver,
) -> miette::Result<DoctorReport> {
    let overrides = crate::project::TargetOverrideArgs {
        source: args.source.clone(),
        install_directory: args.install_directory.clone(),
        frontend: args.frontend.map(zup_core::Frontend::from),
    };
    let selected = crate::project::select_project(&args.manifest, &args.target, &overrides, false)?;
    let manifest_path = selected.manifest_path.clone();
    let manifest_name = selected.manifest_name.clone();
    let runtimes = build_inputs::resolve_runtimes(
        build_inputs::InputMode::Preflight,
        resolver,
        &args.runtime,
        &selected.selected_targets,
    )?;
    let outputs = build_inputs::resolve_outputs(
        build_inputs::InputMode::Preflight,
        build_inputs::Overwrite::Refuse,
        &args.output,
        &manifest_path,
        &selected.manifest.app,
        &selected.selected_targets,
        &runtimes,
    )?;
    let inputs = build_inputs::BuildInputs { runtimes, outputs };
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
        report.elevation();
        if let PlanOutcome::Ready(build) = &plan {
            report.service_runtime(build);
        }
        report.input_problems(build_inputs::InputSubject::Output);
        report.output();
        report.finish()
    }

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
            crate::project::source_policy_for(&config.target),
            zup_build::Writes::None,
        ) {
            Ok(build) => PlanOutcome::Ready(Box::new(build)),
            Err(error) => {
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

    fn hint(&mut self, kind: CheckKind, remedy: &str) {
        if let Some(check) = self
            .checks
            .iter_mut()
            .rev()
            .find(|check| check.kind == kind)
        {
            check.message.push_str(". ");
            check.message.push_str(remedy);
        }
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

    fn runtime_template(&mut self, runtime: &Path) {
        let component = crate::toolchain::runtime_for(&self.config.target, self.config.frontend);
        match zup_toolchain::read(runtime, &component, crate::ZUP_VERSION) {
            Ok(descriptor) => {
                let source = self.inputs.runtimes[self.index]
                    .source
                    .map(|source| format!(" (from the {})", source.as_str()))
                    .unwrap_or_default();
                self.pass(
                    CheckKind::RuntimeTemplate,
                    format!(
                        "runtime template is a zup {} {} component for {}{source}",
                        descriptor.zup_version,
                        descriptor.frontend.as_deref().unwrap_or("runtime"),
                        self.config.target
                    ),
                    Some(runtime),
                );
            }
            Err(error) => {
                self.fail(CheckKind::RuntimeTemplate, error.to_string(), Some(runtime));
                self.hint(CheckKind::RuntimeTemplate, TOOLCHAIN_HINT);
            }
        }
    }

    fn runtime(&mut self) {
        if self
            .inputs
            .problems_for(self.index)
            .any(|problem| problem.subject == build_inputs::InputSubject::Runtime)
        {
            return;
        }
        let Some(runtime) = self.inputs.runtimes[self.index].path.clone() else {
            self.skip(
                CheckKind::RuntimeTemplate,
                "not evaluated: no runtime was resolved",
                None,
            );
            return;
        };
        self.runtime_template(&runtime);
    }

    fn backend(&mut self) {
        let target = &self.config.target;
        match build_inputs::backend_support(target) {
            BackendSupport::Ready => self.pass(
                CheckKind::BuildBackend,
                format!(
                    "the implemented {} backend is ready on this build host for `{target}`",
                    backend_name(target)
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
                    format!(
                        "{} lowering resolved for scope(s) {scopes}",
                        lowering_name(&self.config.target)
                    ),
                    None,
                );
            }
            Err(error) => self.fail(CheckKind::TargetLowering, error.to_string(), None),
        }
    }

    /// This never authenticates: it proves a system `pkexec` is structurally
    /// authorizes is a target-runtime concern. `zup build` never needs this
    fn elevation(&mut self) {
        let target = &self.config.target;
        if target.operating_system() != zup_core::TargetOperatingSystem::Linux {
            self.skip(
                CheckKind::Elevation,
                "elevation is a Linux target concern",
                None,
            );
            return;
        }
        if self.checks.iter().any(|check| {
            check.kind == CheckKind::TargetLowering && check.status == CheckStatus::Fail
        }) {
            self.skip(
                CheckKind::Elevation,
                "not evaluated: target lowering failed",
                None,
            );
            return;
        }
        match self.config.install.scope {
            zup_core::InstallScope::User => {
                self.skip(
                    CheckKind::Elevation,
                    "user scope installs without elevation",
                    None,
                );
            }
            zup_core::InstallScope::Machine | zup_core::InstallScope::Either => {
                if !cfg!(target_os = "linux") {
                    self.skip(
                        CheckKind::Elevation,
                        "machine installation authorizes through `pkexec` on the Linux installing machine",
                        None,
                    );
                    return;
                }
                match system_pkexec() {
                    Some(path) => self.pass(
                        CheckKind::Elevation,
                        format!(
                            "machine installation elevates through `{}`",
                            path.display()
                        ),
                        Some(&path),
                    ),
                    None => self.skip(
                        CheckKind::Elevation,
                        "no system `pkexec` on this machine: machine installation will need one on the installing machine",
                        None,
                    ),
                }
            }
        }
    }

    /// Read-only and never mutating: a probe that names the manager and
    /// reads one property, never an operation that changes unit state. A
    fn service_runtime(&mut self, build: &zup_build::BuildPlan) {
        let target = &self.config.target;
        if target.operating_system() != zup_core::TargetOperatingSystem::Linux {
            self.skip(
                CheckKind::ServiceRuntime,
                "service runtime is a Linux target concern",
                None,
            );
            return;
        }
        if self.checks.iter().any(|check| {
            check.kind == CheckKind::TargetLowering && check.status == CheckStatus::Fail
        }) {
            self.skip(
                CheckKind::ServiceRuntime,
                "not evaluated: target lowering failed",
                None,
            );
            return;
        }
        let services: usize = build
            .targets
            .iter()
            .map(|plan| plan.installer.services.len())
            .sum();
        if services == 0 {
            self.skip(
                CheckKind::ServiceRuntime,
                "no services are declared for this target",
                None,
            );
            return;
        }
        if !cfg!(target_os = "linux") {
            self.skip(
                CheckKind::ServiceRuntime,
                "machine services run through the systemd system manager on the Linux installing machine",
                None,
            );
            return;
        }
        match system_systemd() {
            SystemdReadiness::Ready(version) => self.pass(
                CheckKind::ServiceRuntime,
                format!("systemd {version} system manager answers on this machine"),
                None,
            ),
            SystemdReadiness::TooOld(version) => self.fail(
                CheckKind::ServiceRuntime,
                systemd_too_old_message(version),
                None,
            ),
            SystemdReadiness::Unknown => self.skip(
                CheckKind::ServiceRuntime,
                "no systemd system manager on this machine: machine services need one on the installing machine",
                None,
            ),
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

/// The target's own platform, never the build host's: a Windows host building
fn backend_name(target: &zup_core::TargetTriple) -> &'static str {
    match target.operating_system() {
        zup_core::TargetOperatingSystem::Windows => "Windows",
        zup_core::TargetOperatingSystem::Linux => "Linux",
        _ => "platform",
    }
}

fn lowering_name(target: &zup_core::TargetTriple) -> &'static str {
    match target.operating_system() {
        zup_core::TargetOperatingSystem::Windows => "Windows",
        zup_core::TargetOperatingSystem::Linux => "Linux",
        _ => "target",
    }
}

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

/// Presence, ownership, and writability - never execution, which would
#[cfg(target_os = "linux")]
fn system_pkexec() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ["/usr/bin/pkexec", "/bin/pkexec"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.uid() == 0
                    && metadata.mode() & 0o022 == 0
                    && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// host reports by skipping, never by failing the project.
#[cfg(not(target_os = "linux"))]
fn system_pkexec() -> Option<std::path::PathBuf> {
    None
}

#[cfg(target_os = "linux")]
fn system_systemd() -> SystemdReadiness {
    let mut manager = match zup_linux::RealSystemd::connect() {
        Ok(manager) => manager,
        Err(_) => return SystemdReadiness::Unknown,
    };
    SystemdReadiness::of_version(manager.version().ok().as_deref())
}

/// Linux: a non-Linux host never constructs `TooOld`, so its copy just
#[cfg(target_os = "linux")]
fn systemd_too_old_message(version: u32) -> String {
    format!(
        "systemd {version} is older than the minimum {} for `Type=exec` service units",
        zup_linux::MINIMUM_SYSTEMD_VERSION,
    )
}

/// Linux: a non-Linux host never constructs `TooOld`, so its copy just
#[cfg(not(target_os = "linux"))]
fn systemd_too_old_message(version: u32) -> String {
    format!("systemd {version} is too old for `Type=exec` service units")
}
#[allow(dead_code)]
enum SystemdReadiness {
    Ready(u32),
    TooOld(u32),
    Unknown,
}

impl SystemdReadiness {
    #[cfg(target_os = "linux")]
    fn of_version(version: Option<&str>) -> Self {
        match version.and_then(zup_linux::parse_manager_version) {
            Some(major) if major >= zup_linux::MINIMUM_SYSTEMD_VERSION => Self::Ready(major),
            Some(major) => Self::TooOld(major),
            None => Self::Unknown,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod readiness_tests {
    use super::SystemdReadiness;

    #[test]
    fn versions_verdict_by_the_exec_baseline() {
        assert!(matches!(
            SystemdReadiness::of_version(Some("259")),
            SystemdReadiness::Ready(259)
        ));
        assert!(matches!(
            SystemdReadiness::of_version(Some("259.5-0ubuntu3.4")),
            SystemdReadiness::Ready(259)
        ));
        assert!(matches!(
            SystemdReadiness::of_version(Some("240")),
            SystemdReadiness::Ready(240)
        ));
        assert!(matches!(
            SystemdReadiness::of_version(Some("239")),
            SystemdReadiness::TooOld(239)
        ));
        assert!(matches!(
            SystemdReadiness::of_version(Some("not-a-version")),
            SystemdReadiness::Unknown
        ));
        assert!(matches!(
            SystemdReadiness::of_version(None),
            SystemdReadiness::Unknown
        ));
    }
}

/// build host reports by skipping, never by failing the project.
#[cfg(not(target_os = "linux"))]
fn system_systemd() -> SystemdReadiness {
    SystemdReadiness::Unknown
}

#[cfg(unix)]
fn directory_accepts_writes(directory: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    Ok(std::fs::metadata(directory)?.permissions().mode() & 0o222 != 0)
}

#[cfg(not(any(windows, unix)))]
fn directory_accepts_writes(directory: &Path) -> io::Result<bool> {
    Ok(!std::fs::metadata(directory)?.permissions().readonly())
}
