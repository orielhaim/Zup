use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{App, ResolvedTargetConfig, SelectedScope, TargetOperatingSystem, TargetTriple};

use crate::toolchain::{ToolchainResolver, ToolchainSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Enforce,
    Preflight,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overwrite {
    #[default]
    Refuse,
    Force,
}

impl From<bool> for Overwrite {
    fn from(force: bool) -> Self {
        if force { Self::Force } else { Self::Refuse }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSubject {
    Runtime,
    Output,
}

#[derive(Debug, Clone)]
pub struct InputProblem {
    pub subject: InputSubject,
    pub message: String,
    pub path: Option<PathBuf>,
}

impl InputProblem {
    fn new(subject: InputSubject, message: String, path: Option<PathBuf>) -> Self {
        Self {
            subject,
            message,
            path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSlot {
    pub path: Option<PathBuf>,
    pub source: Option<ToolchainSource>,
    pub problems: Vec<InputProblem>,
}

#[derive(Debug, Clone)]
pub struct OutputSlot {
    pub path: PathBuf,
    pub derived: bool,
    pub problems: Vec<InputProblem>,
}

#[derive(Debug, Clone)]
pub struct BuildInputs {
    pub runtimes: Vec<RuntimeSlot>,
    pub outputs: Vec<OutputSlot>,
}

impl BuildInputs {
    pub fn problems_for(&self, index: usize) -> impl Iterator<Item = &InputProblem> {
        self.runtimes
            .get(index)
            .into_iter()
            .flat_map(|slot| slot.problems.iter())
            .chain(
                self.outputs
                    .get(index)
                    .into_iter()
                    .flat_map(|slot| slot.problems.iter()),
            )
    }
}

pub fn resolve_runtimes(
    mode: InputMode,
    resolver: &ToolchainResolver,
    supplied: &[PathBuf],
    targets: &[ResolvedTargetConfig],
) -> miette::Result<Vec<RuntimeSlot>> {
    let slots = resolve_runtime_slots(resolver, supplied, targets);
    if let (InputMode::Enforce, Some(problem)) = (mode, first_problem(&slots, &[])) {
        return Err(miette::miette!("{}", problem.message));
    }
    Ok(slots)
}

pub fn resolve_outputs(
    mode: InputMode,
    overwrite: Overwrite,
    supplied: &[PathBuf],
    manifest_path: &Path,
    app: &App,
    targets: &[ResolvedTargetConfig],
    runtimes: &[RuntimeSlot],
) -> miette::Result<Vec<OutputSlot>> {
    let slots = inspect_output_slots(overwrite, supplied, manifest_path, app, targets, runtimes);
    if let (InputMode::Enforce, Some(problem)) = (mode, first_problem(&[], &slots)) {
        return Err(miette::miette!("{}", problem.message));
    }
    Ok(slots)
}

fn first_problem<'a>(
    runtimes: &'a [RuntimeSlot],
    outputs: &'a [OutputSlot],
) -> Option<&'a InputProblem> {
    runtimes
        .iter()
        .flat_map(|slot| slot.problems.iter())
        .chain(outputs.iter().flat_map(|slot| slot.problems.iter()))
        .next()
}

fn inspect_output_slots(
    overwrite: Overwrite,
    outputs: &[PathBuf],
    manifest_path: &Path,
    app: &App,
    targets: &[ResolvedTargetConfig],
    runtimes: &[RuntimeSlot],
) -> Vec<OutputSlot> {
    let mut outputs = resolve_output_slots(outputs, manifest_path, app, targets);
    let resolved_runtimes = runtimes
        .iter()
        .filter_map(|slot| slot.path.clone())
        .collect::<Vec<_>>();
    for slot in &mut outputs {
        if overwrite == Overwrite::Refuse && slot.path.exists() {
            slot.problems.push(InputProblem::new(
                InputSubject::Output,
                format!(
                    "output `{}` already exists; pass --force to overwrite it",
                    slot.path.display()
                ),
                Some(slot.path.clone()),
            ));
        }
        if resolved_runtimes
            .iter()
            .any(|runtime| normalized_path(runtime) == normalized_path(&slot.path))
        {
            slot.problems.push(InputProblem::new(
                InputSubject::Output,
                format!(
                    "output `{}` is also a selected runtime",
                    slot.path.display()
                ),
                Some(slot.path.clone()),
            ));
        }
    }
    outputs
}

pub fn cardinality_problem(
    targets: &[ResolvedTargetConfig],
    received: usize,
    noun: &str,
    flag: &str,
) -> String {
    let names = targets
        .iter()
        .map(|target| target.profile.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "selected {} targets ({names}) but received {received} {noun}; provide one {flag} per target, in that order",
        targets.len()
    )
}

pub fn align_per_target<'a, T>(
    noun: &str,
    flag: &str,
    supplied: &'a [T],
    targets: &[ResolvedTargetConfig],
) -> miette::Result<Option<&'a [T]>> {
    if supplied.is_empty() {
        return Ok(None);
    }
    if supplied.len() != targets.len() {
        return Err(miette::miette!(
            "{}",
            cardinality_problem(targets, supplied.len(), noun, flag)
        ));
    }
    Ok(Some(supplied))
}

fn resolve_runtime_slots(
    resolver: &ToolchainResolver,
    supplied: &[PathBuf],
    targets: &[ResolvedTargetConfig],
) -> Vec<RuntimeSlot> {
    let mismatched = !supplied.is_empty() && supplied.len() != targets.len();
    let mut seen = BTreeMap::<PathBuf, usize>::new();
    let mut slots = Vec::with_capacity(targets.len());
    for index in 0..targets.len() {
        let mut problems = Vec::new();
        if mismatched {
            problems.push(InputProblem::new(
                InputSubject::Runtime,
                cardinality_problem(targets, supplied.len(), "runtimes", "--runtime"),
                None,
            ));
        }
        let component =
            crate::toolchain::runtime_for(&targets[index].target, targets[index].frontend);
        match resolver.resolve(&component, supplied.get(index).map(|path| path.as_path())) {
            Ok(resolved) => {
                if let Some(previous) = seen.insert(resolved.path.clone(), index)
                    && previous != index
                {
                    problems.push(InputProblem::new(
                        InputSubject::Runtime,
                        format!(
                            "runtime `{}` was supplied for more than one target",
                            resolved.path.display()
                        ),
                        Some(resolved.path.clone()),
                    ));
                }
                slots.push(RuntimeSlot {
                    path: Some(resolved.path),
                    source: Some(resolved.source),
                    problems,
                });
            }
            Err(error) => {
                problems.push(InputProblem::new(
                    InputSubject::Runtime,
                    crate::toolchain::missing_component_message(&component, &error),
                    supplied.get(index).cloned(),
                ));
                slots.push(RuntimeSlot {
                    path: None,
                    source: None,
                    problems,
                });
            }
        }
    }
    slots
}

fn resolve_output_slots(
    supplied: &[PathBuf],
    manifest_path: &Path,
    app: &App,
    targets: &[ResolvedTargetConfig],
) -> Vec<OutputSlot> {
    let app_name = sanitized_file_stem(app.name.as_str(), app.id.as_str());
    let mut used = BTreeMap::<PathBuf, ()>::new();
    let mut slots = Vec::with_capacity(targets.len());
    let mismatched = !supplied.is_empty() && supplied.len() != targets.len();
    let mut seen = BTreeMap::<PathBuf, usize>::new();
    for (index, target) in targets.iter().enumerate() {
        let mut problems = Vec::new();
        if mismatched {
            problems.push(InputProblem::new(
                InputSubject::Output,
                cardinality_problem(targets, supplied.len(), "outputs", "--output"),
                None,
            ));
        }
        let (path, derived) = match supplied.get(index) {
            Some(path) => (path.clone(), false),
            None => (
                derived_output_path(&app_name, manifest_path, targets, target, &mut used),
                true,
            ),
        };
        if seen.insert(normalized_path(&path), index).is_some() {
            problems.push(InputProblem::new(
                InputSubject::Output,
                format!(
                    "output `{}` is used by more than one target",
                    path.display()
                ),
                Some(path.clone()),
            ));
        }
        slots.push(OutputSlot {
            path,
            derived,
            problems,
        });
    }
    slots
}

fn derived_output_path(
    app_name: &str,
    manifest_path: &Path,
    targets: &[ResolvedTargetConfig],
    target: &ResolvedTargetConfig,
    used: &mut BTreeMap<PathBuf, ()>,
) -> PathBuf {
    let parent = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let suffix = target.target.executable_suffix();
    let stem = if targets.len() == 1 {
        format!("{app_name}-Setup")
    } else {
        format!(
            "{app_name}-Setup-{}",
            sanitized_file_stem(target.profile.as_str(), "target")
        )
    };
    let mut file_name = format!("{stem}{suffix}");
    let mut candidate = parent.join(&file_name);
    if targets.len() > 1 {
        let mut number = 2;
        while used.contains_key(&normalized_path(&candidate)) {
            file_name = format!("{stem}-{number}{suffix}");
            candidate = parent.join(&file_name);
            number += 1;
        }
    }
    used.insert(normalized_path(&candidate), ());
    candidate
}

pub fn default_build_target() -> String {
    if cfg!(target_os = "linux") {
        return crate::linux_support::SUPPORTED_LINUX_TARGET.to_owned();
    }
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64-pc-windows-msvc".to_owned()
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        "x86_64-pc-windows-msvc".to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSupport {
    Ready,
    HostBackendUnavailable,
    NotImplemented,
}

impl BackendSupport {
    pub fn reason(self, target: &TargetTriple) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::HostBackendUnavailable => Some(format!(
                "backend unavailable: lowering `{target}` requires a {} build host",
                platform_name(target.operating_system())
            )),
            Self::NotImplemented => Some(
                "backend not implemented: no implemented backend answers for this target's \
                 platform"
                    .to_owned(),
            ),
        }
    }

    pub fn build_error(self, target: &TargetTriple) -> Option<String> {
        self.reason(target)
            .map(|reason| format!("unsupported backend for target `{target}`: {reason}"))
    }
}

pub fn backend_support(target: &TargetTriple) -> BackendSupport {
    let implemented = matches!(
        target.operating_system(),
        TargetOperatingSystem::Windows | TargetOperatingSystem::Linux
    );
    if !implemented {
        return BackendSupport::NotImplemented;
    }
    if host_can_lower(target.operating_system()) {
        BackendSupport::Ready
    } else {
        BackendSupport::HostBackendUnavailable
    }
}

const fn host_can_lower(os: TargetOperatingSystem) -> bool {
    matches!(os, TargetOperatingSystem::Linux)
        || (matches!(os, TargetOperatingSystem::Windows) && cfg!(windows))
}

fn platform_name(os: TargetOperatingSystem) -> String {
    match os {
        TargetOperatingSystem::Windows => "windows".to_owned(),
        TargetOperatingSystem::Linux => "linux".to_owned(),
        other => other.to_string(),
    }
}

/// the source tree: an unsupported target must not cost a source-tree walk or a
pub fn check_backend_support(config: &ResolvedTargetConfig) -> miette::Result<()> {
    match backend_support(&config.target).build_error(&config.target) {
        Some(error) => Err(miette::miette!("{error}")),
        None => Ok(()),
    }
}

/// The dispatch is on the *target's* operating system, never on the build
pub fn check_target_lowering(
    build: &zup_build::BuildPlan,
    config: &ResolvedTargetConfig,
) -> miette::Result<Vec<SelectedScope>> {
    check_backend_support(config)?;
    match config.target.operating_system() {
        TargetOperatingSystem::Linux => check_linux_lowering(build, config),
        _ => check_windows_lowering(build, config),
    }
}

fn check_linux_lowering(
    build: &zup_build::BuildPlan,
    config: &ResolvedTargetConfig,
) -> miette::Result<Vec<SelectedScope>> {
    let plan = build
        .target_by_triple(&config.target)
        .ok_or_else(|| miette::miette!("no materialized plan for target `{}`", config.target))?;
    let errors = crate::linux_support::linux_capability_errors(config, plan);
    if !errors.is_empty() {
        return Err(miette::miette!("{}", errors.join("\n")));
    }
    let scopes = match config.install.scope {
        zup_core::InstallScope::User => vec![SelectedScope::User],
        zup_core::InstallScope::Machine => vec![SelectedScope::Machine],
        zup_core::InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };
    #[cfg(target_os = "linux")]
    {
        for scope in &scopes {
            let request = zup_plan::PlanRequest::new(config.target.clone(), *scope);
            let install = zup_plan::plan_without_plugins(build, &request).map_err(|error| {
                miette::miette!("semantic plan for target `{}`: {error}", config.target)
            })?;
            zup_linux::resolve_target(
                &install,
                &zup_linux::LinuxInstallLocationResolver::default(),
            )
            .map_err(|error| {
                miette::miette!("Linux lowering for target `{}`: {error}", config.target)
            })?;
        }
    }
    Ok(scopes)
}

fn check_windows_lowering(
    build: &zup_build::BuildPlan,
    config: &ResolvedTargetConfig,
) -> miette::Result<Vec<SelectedScope>> {
    let scopes = match config.install.scope {
        zup_core::InstallScope::User => vec![SelectedScope::User],
        zup_core::InstallScope::Machine => vec![SelectedScope::Machine],
        zup_core::InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };

    #[cfg(not(windows))]
    {
        let _ = (build, scopes);
        Err(miette::miette!(
            "Windows lowering for target `{}` requires a Windows build host",
            config.target
        ))
    }

    #[cfg(windows)]
    {
        for scope in &scopes {
            let request = zup_plan::PlanRequest::new(config.target.clone(), *scope);
            let install = zup_plan::plan_without_plugins(build, &request).map_err(|error| {
                miette::miette!("semantic plan for target `{}`: {error}", config.target)
            })?;
            zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(*scope))
                .map_err(|error| {
                    miette::miette!("Windows lowering for target `{}`: {error}", config.target)
                })?;
        }
        Ok(scopes)
    }
}

pub fn sanitized_file_stem(value: &str, fallback: &str) -> String {
    let name = value
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '-'
            } else {
                character
            }
        })
        .collect::<String>();
    let name = name.trim_matches([' ', '.']);
    if name.is_empty() {
        fallback.into()
    } else {
        name.to_owned()
    }
}

pub fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub fn normalized_path(path: &Path) -> PathBuf {
    let absolute = absolute_path(path);
    let absolute = absolute.canonicalize().unwrap_or(absolute);
    #[cfg(windows)]
    {
        PathBuf::from(absolute.to_string_lossy().to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        absolute
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::Frontend;

    #[test]
    fn backend_boundary_is_explicit_for_every_target_class() {
        let windows = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let linux = TargetTriple::parse("aarch64-unknown-linux-gnu").unwrap();
        let macos = TargetTriple::parse("aarch64-apple-darwin").unwrap();
        assert_eq!(backend_support(&macos), BackendSupport::NotImplemented);
        assert!(
            backend_support(&macos)
                .reason(&macos)
                .unwrap()
                .contains("backend not implemented"),
            "a platform no backend owns has no backend *anywhere*, which is a different answer \
             from one this host cannot run"
        );
        assert!(
            backend_support(&macos)
                .build_error(&macos)
                .unwrap()
                .starts_with("unsupported backend for target")
        );
        let windows_support = backend_support(&windows);
        assert_eq!(
            windows_support,
            if cfg!(windows) {
                BackendSupport::Ready
            } else {
                BackendSupport::HostBackendUnavailable
            },
            "a Windows target is a question about the build host, not about whether a backend \
             exists"
        );
        if let Some(reason) = windows_support.reason(&windows) {
            assert!(reason.contains("backend unavailable"), "{reason}");
            assert!(
                !reason.contains("WindowsBackend"),
                "the diagnostic names the host and the target, never a variant"
            );
        }

        let linux_support = backend_support(&linux);
        assert_eq!(linux_support, BackendSupport::Ready);
        assert!(
            linux_support.reason(&linux).is_none(),
            "a ready backend has no reason"
        );
    }

    fn empty_resolver() -> ToolchainResolver {
        ToolchainResolver::new(
            "0.0.0-test".to_owned(),
            PathBuf::from("C:/nowhere/zup.exe"),
            PathBuf::from("C:/nowhere/state"),
        )
    }

    #[test]
    fn a_target_with_no_resolvable_component_reports_the_refusal_and_enforces_it() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let resolver = empty_resolver();
        let slots = resolve_runtimes(InputMode::Preflight, &resolver, &[], &targets)
            .expect("preflight reports rather than raises");
        assert_eq!(slots.len(), 2);
        for slot in &slots {
            assert!(slot.path.is_none());
            assert_eq!(slot.problems.len(), 1, "{:?}", slot.problems);
            assert!(
                slot.problems[0]
                    .message
                    .contains("cargo xtask toolchain build"),
                "{}",
                slot.problems[0].message
            );
        }
        let error = resolve_runtimes(InputMode::Enforce, &resolver, &[], &targets)
            .expect_err("a build refuses to start without a component")
            .to_string();
        assert!(error.contains("cargo xtask toolchain build"), "{error}");
    }

    #[test]
    fn every_repeatable_per_target_flag_uses_one_alignment_message() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let empty: [&str; 0] = [];
        assert!(
            align_per_target("runtimes", "--runtime", &empty, &targets)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            align_per_target("runtimes", "--runtime", &["a", "b"], &targets)
                .unwrap()
                .map(<[&str]>::len),
            Some(2)
        );
        for (noun, flag) in [
            ("runtimes", "--runtime"),
            ("outputs", "--output"),
            ("sources", "--source"),
            ("install directories", "--install-directory"),
        ] {
            let error = align_per_target(noun, flag, &["one"], &targets)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                format!(
                    "selected 2 targets (alpha, beta) but received 1 {noun}; provide one {flag} per target, in that order"
                )
            );
            let surplus = align_per_target(noun, flag, &["one", "two", "three"], &targets)
                .unwrap_err()
                .to_string();
            assert_eq!(
                surplus,
                format!(
                    "selected 2 targets (alpha, beta) but received 3 {noun}; provide one {flag} per target, in that order"
                ),
                "the profiles are named because the order is not obvious"
            );
        }
    }

    #[test]
    fn a_derived_output_is_named_for_its_target_and_the_run_is_left_to_the_caller() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let manifest = Path::new("/tmp/project/zup.toml");
        let outputs = inspect_output_slots(Overwrite::Refuse, &[], manifest, &app(), &targets, &[]);
        let names: std::collections::BTreeSet<String> = outputs
            .iter()
            .map(|slot| slot.path.display().to_string())
            .collect();
        assert_eq!(names.len(), outputs.len(), "derived names must not collide");
        assert!(outputs.iter().all(|slot| slot.derived));

        let single =
            inspect_output_slots(Overwrite::Refuse, &[], manifest, &app(), &targets[..1], &[]);
        assert_eq!(single.len(), 1);
        assert!(single[0].derived);
        assert!(
            single[0].path.ends_with("Doctor App-Setup.exe"),
            "a single target gets no profile suffix: {}",
            single[0].path.display()
        );
    }

    #[test]
    fn an_existing_output_is_refused_unless_it_may_be_overwritten() {
        let existing = tempfile::NamedTempFile::new().unwrap();
        let manifest = Path::new("/tmp/project/zup.toml");
        let output = existing.path().to_path_buf();
        let targets = [target_config("alpha")];

        let refused = inspect_output_slots(
            Overwrite::Refuse,
            std::slice::from_ref(&output),
            manifest,
            &app(),
            &targets,
            &[],
        );
        let problem = refused[0]
            .problems
            .iter()
            .find(|problem| problem.message.contains("already exists"))
            .expect("an existing output is a problem");
        assert!(
            problem.message.contains("--force"),
            "the refusal names the escape hatch: {}",
            problem.message
        );

        let forced = inspect_output_slots(
            Overwrite::Force,
            std::slice::from_ref(&output),
            manifest,
            &app(),
            &targets,
            &[],
        );
        assert!(
            forced[0].problems.is_empty(),
            "--force permits replacing an existing output: {:?}",
            forced[0].problems
        );
    }

    fn app() -> App {
        App {
            id: zup_core::AppId::new("com.example.doctor").unwrap(),
            name: zup_core::NonEmptyString::new("Doctor App").unwrap(),
            version: semver::Version::parse("1.0.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        }
    }

    fn target_config(profile: &str) -> ResolvedTargetConfig {
        ResolvedTargetConfig {
            profile: zup_core::TargetProfileId::new(profile).unwrap(),
            target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            source: zup_core::Source::new(PathBuf::from("dist")).unwrap(),
            frontend: Frontend::Gui,
            install: zup_core::Install {
                scope: zup_core::InstallScope::User,
                directory: zup_core::InstallDirectory {
                    user: None,
                    machine: None,
                },
                allow_directory_override: false,
            },
        }
    }
}
