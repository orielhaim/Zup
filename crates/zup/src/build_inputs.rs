//! Shared build-input resolution for `zup build` and `zup doctor`.
//!
//! Both commands map every selected target profile to exactly one runtime
//! template and one output path. That mapping lives here once, so the readiness
//! report describes the inputs `zup build` will actually use instead of
//! re-deriving names and re-implementing the checks.
//!
//! [`InputMode::Enforce`] fails on the first problem so `zup build` writes
//! nothing. [`InputMode::Preflight`] records every problem against its target
//! so `zup doctor` can report all of them in one pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{
    App, Frontend, ResolvedTargetConfig, SelectedScope, TargetOperatingSystem, TargetTriple,
};
use zup_windows::PeSubsystem;

/// How strictly shared input resolution reports a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// `zup build`: the first problem aborts before an artifact is written.
    Enforce,
    /// `zup doctor`: every problem is reported and checking continues.
    Preflight,
}

/// Whether an existing output directory may be replaced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// Refuse to write over an output that already exists.
    #[default]
    Refuse,
    /// Replace an output that already exists.
    Force,
}

impl From<bool> for Overwrite {
    /// `--force` selects [`Self::Force`]; its absence selects [`Self::Refuse`].
    fn from(force: bool) -> Self {
        if force { Self::Force } else { Self::Refuse }
    }
}

/// Which resolved input a problem belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSubject {
    Runtime,
    Output,
}

/// A problem found while mapping inputs onto targets.
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

/// One target's runtime template, as far as it could be resolved.
#[derive(Debug, Clone)]
pub struct RuntimeSlot {
    /// `None` when no runtime could be assigned to this target.
    pub path: Option<PathBuf>,
    /// True when the runtime was discovered next to this executable.
    pub discovered: bool,
    pub problems: Vec<InputProblem>,
}

/// One target's installer output, as far as it could be resolved.
#[derive(Debug, Clone)]
pub struct OutputSlot {
    pub path: PathBuf,
    /// True when no `--output` was supplied and the name was derived.
    pub derived: bool,
    pub problems: Vec<InputProblem>,
}

/// Resolved inputs for every selected target, in selection order.
#[derive(Debug, Clone)]
pub struct BuildInputs {
    pub runtimes: Vec<RuntimeSlot>,
    pub outputs: Vec<OutputSlot>,
}

impl BuildInputs {
    /// The first problem in target order, runtime before output.
    pub fn first_problem(&self) -> Option<&InputProblem> {
        self.problems().next()
    }

    /// Every problem, in target order and runtime before output.
    pub fn problems(&self) -> impl Iterator<Item = &InputProblem> {
        self.runtimes
            .iter()
            .flat_map(|slot| slot.problems.iter())
            .chain(self.outputs.iter().flat_map(|slot| slot.problems.iter()))
    }

    /// Problems recorded for one target.
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

/// Resolve the runtime and output inputs for every selected target.
///
/// `Enforce` returns the first problem as an error. `Preflight` returns every
/// slot with its problems attached, so the caller can report all of them.
pub fn resolve_build_inputs(
    mode: InputMode,
    overwrite: Overwrite,
    runtimes: &[PathBuf],
    outputs: &[PathBuf],
    manifest_path: &Path,
    app: &App,
    targets: &[ResolvedTargetConfig],
) -> miette::Result<BuildInputs> {
    let inputs = inspect_build_inputs(overwrite, runtimes, outputs, manifest_path, app, targets);
    if let (InputMode::Enforce, Some(problem)) = (mode, inputs.first_problem()) {
        return Err(miette::miette!("{}", problem.message));
    }
    Ok(inputs)
}

/// Total input resolution: every problem is recorded, none are raised.
fn inspect_build_inputs(
    overwrite: Overwrite,
    runtimes: &[PathBuf],
    outputs: &[PathBuf],
    manifest_path: &Path,
    app: &App,
    targets: &[ResolvedTargetConfig],
) -> BuildInputs {
    let runtimes = resolve_runtime_slots(runtimes, targets);
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
    BuildInputs { runtimes, outputs }
}

/// The diagnostic for a repeatable per-target flag that does not line up with
/// the selected targets. One value per target, or none at all.
pub fn cardinality_problem(targets: usize, received: usize, noun: &str, flag: &str) -> String {
    format!(
        "selected {targets} targets but received {received} {noun}; provide one {flag} per target"
    )
}

/// Align one repeatable per-target flag against the selected target count.
///
/// An empty flag means the manifest decides. Otherwise exactly one value per
/// target is required, so a single value against several targets is an error
/// rather than a silent broadcast. Every repeatable per-target flag reports a
/// mismatch through this one message.
pub fn align_per_target<'a, T>(
    noun: &str,
    flag: &str,
    supplied: &'a [T],
    targets: usize,
) -> miette::Result<Option<&'a [T]>> {
    if supplied.is_empty() {
        return Ok(None);
    }
    if supplied.len() != targets {
        return Err(miette::miette!(
            "{}",
            cardinality_problem(targets, supplied.len(), noun, flag)
        ));
    }
    Ok(Some(supplied))
}

fn resolve_runtime_slots(
    supplied: &[PathBuf],
    targets: &[ResolvedTargetConfig],
) -> Vec<RuntimeSlot> {
    if supplied.is_empty() {
        if targets.len() != 1 {
            return vec![
                RuntimeSlot {
                    path: None,
                    discovered: false,
                    problems: vec![InputProblem::new(
                        InputSubject::Runtime,
                        "multiple targets require one explicit --runtime for each selected target"
                            .to_owned(),
                        None,
                    )],
                };
                targets.len()
            ];
        }
        return match discover_runtime(targets[0].frontend) {
            Ok(path) => vec![RuntimeSlot {
                path: Some(path),
                discovered: true,
                problems: Vec::new(),
            }],
            Err(error) => vec![RuntimeSlot {
                path: None,
                discovered: true,
                problems: vec![InputProblem::new(
                    InputSubject::Runtime,
                    error.to_string(),
                    None,
                )],
            }],
        };
    }

    let mismatched = supplied.len() != targets.len();
    let mut seen = BTreeMap::<PathBuf, usize>::new();
    let mut slots = Vec::with_capacity(targets.len());
    for index in 0..targets.len() {
        let mut problems = Vec::new();
        if mismatched {
            problems.push(InputProblem::new(
                InputSubject::Runtime,
                cardinality_problem(targets.len(), supplied.len(), "runtimes", "--runtime"),
                None,
            ));
        }
        let Some(requested) = supplied.get(index) else {
            slots.push(RuntimeSlot {
                path: None,
                discovered: false,
                problems,
            });
            continue;
        };
        let path = match requested.canonicalize() {
            Ok(path) => {
                if seen.insert(path.clone(), index).is_some() {
                    problems.push(InputProblem::new(
                        InputSubject::Runtime,
                        format!(
                            "runtime `{}` was supplied for more than one target",
                            path.display()
                        ),
                        Some(path.clone()),
                    ));
                }
                Some(path)
            }
            Err(error) => {
                problems.push(InputProblem::new(
                    InputSubject::Runtime,
                    format!("runtime {}: {error}", index + 1),
                    Some(requested.clone()),
                ));
                None
            }
        };
        slots.push(RuntimeSlot {
            path,
            discovered: false,
            problems,
        });
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
                cardinality_problem(targets.len(), supplied.len(), "outputs", "--output"),
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
    let name = if targets.len() == 1 {
        format!("{app_name}-Setup.exe")
    } else {
        format!(
            "{app_name}-Setup-{}.exe",
            sanitized_file_stem(target.profile.as_str(), "target")
        )
    };
    let mut file_name = name.clone();
    let mut candidate = parent.join(&file_name);
    if targets.len() > 1 {
        let base = name.trim_end_matches(".exe").to_owned();
        let mut suffix = 2;
        while used.contains_key(&normalized_path(&candidate)) {
            file_name = format!("{base}-{suffix}.exe");
            candidate = parent.join(&file_name);
            suffix += 1;
        }
    }
    used.insert(normalized_path(&candidate), ());
    candidate
}

/// The runtime template file name for one installer frontend.
pub fn runtime_template_name(frontend: Frontend) -> &'static str {
    match frontend {
        Frontend::Gui => "zup-setup-gui",
        Frontend::Console => "zup-setup-console",
        Frontend::Headless => "zup-setup-headless",
    }
}

/// The PE subsystem an installer runtime must declare for one frontend.
pub fn expected_subsystem(frontend: Frontend) -> PeSubsystem {
    match frontend {
        Frontend::Gui => PeSubsystem::Gui,
        Frontend::Console | Frontend::Headless => PeSubsystem::Console,
    }
}

/// Whether a runtime executable is the template for `frontend`.
pub fn validate_runtime_template(path: &Path, frontend: Frontend) -> miette::Result<()> {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let expected = runtime_template_name(frontend);
    let gui_alias = frontend == Frontend::Gui && stem == "zup-setup";
    if stem == expected || gui_alias {
        Ok(())
    } else {
        Err(miette::miette!(
            "runtime `{}` is not the {frontend} template",
            path.display()
        ))
    }
}

/// Find the runtime template for one frontend next to this executable.
pub fn discover_runtime(frontend: Frontend) -> miette::Result<PathBuf> {
    let current =
        zup_windows::current_exe().map_err(|error| miette::miette!("runtime: {error}"))?;
    let name = runtime_template_name(frontend);
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let mut setup = current.with_file_name(&name);
    if frontend == Frontend::Gui && !setup.exists() {
        setup = current.with_file_name(if cfg!(windows) {
            "zup-setup.exe"
        } else {
            "zup-setup"
        });
    }
    setup.canonicalize().map_err(|error| {
        miette::miette!(
            "{frontend:?} runtime `{}` is unavailable next to `{}` ({error}); build the selected runtime template or pass --runtime",
            setup.display(),
            current.display()
        )
    })
}

/// The default target triple for a new manifest on this build host.
pub fn default_build_target() -> String {
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64-pc-windows-msvc".to_owned()
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        "x86_64-pc-windows-msvc".to_owned()
    }
}

/// Whether the current build host can run the implemented Windows backend.
pub const fn windows_backend_available() -> bool {
    cfg!(windows)
}

/// Backend support for one target on this build host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSupport {
    /// The implemented Windows backend can build this target here.
    Ready,
    /// A Windows target on a host that cannot lower Windows targets.
    WindowsBackendUnavailable,
    /// A target whose platform backend is not implemented.
    NotImplemented,
}

impl BackendSupport {
    /// Why this host cannot build the target, or `None` when it can.
    pub fn reason(self, target: &TargetTriple) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::WindowsBackendUnavailable => Some(format!(
                "backend unavailable: Windows target lowering for `{target}` requires a Windows build host"
            )),
            Self::NotImplemented => Some(
                "backend not implemented: only Windows targets have an implemented backend"
                    .to_owned(),
            ),
        }
    }

    /// The same boundary as a hard `zup build` error.
    pub fn build_error(self, target: &TargetTriple) -> Option<String> {
        self.reason(target)
            .map(|reason| format!("unsupported backend for target `{target}`: {reason}"))
    }
}

/// Classify one target against the backends this host implements.
pub fn backend_support(target: &TargetTriple) -> BackendSupport {
    if target.operating_system() != TargetOperatingSystem::Windows {
        return BackendSupport::NotImplemented;
    }
    if windows_backend_available() {
        BackendSupport::Ready
    } else {
        BackendSupport::WindowsBackendUnavailable
    }
}

/// Reject a target this build host has no backend for.
///
/// This reads nothing from disk, so a build path can call it before it walks
/// the source tree: an unsupported target must not cost a source-tree walk or a
/// prerequisite resolution first.
pub fn check_backend_support(config: &ResolvedTargetConfig) -> miette::Result<()> {
    match backend_support(&config.target).build_error(&config.target) {
        Some(error) => Err(miette::miette!("{error}")),
        None => Ok(()),
    }
}

/// Whether Windows target lowering resolves for every install scope of a target.
pub fn check_target_lowering(
    build: &zup_build::BuildPlan,
    config: &ResolvedTargetConfig,
) -> miette::Result<Vec<SelectedScope>> {
    check_backend_support(config)?;
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

/// A filesystem-safe file stem derived from an application or profile name.
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

/// The absolute form of a possibly relative path.
pub fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// A comparison key for one path, case-folded where the filesystem is.
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

    #[test]
    fn backend_boundary_is_explicit_for_every_target_class() {
        let windows = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let linux = TargetTriple::parse("aarch64-unknown-linux-gnu").unwrap();
        let macos = TargetTriple::parse("aarch64-apple-darwin").unwrap();
        assert_eq!(backend_support(&linux), BackendSupport::NotImplemented);
        assert_eq!(backend_support(&macos), BackendSupport::NotImplemented);
        assert!(
            backend_support(&linux)
                .reason(&linux)
                .unwrap()
                .contains("backend not implemented")
        );
        assert!(
            backend_support(&linux)
                .build_error(&linux)
                .unwrap()
                .starts_with("unsupported backend for target")
        );
        let windows_support = backend_support(&windows);
        if windows_backend_available() {
            assert_eq!(windows_support, BackendSupport::Ready);
            assert!(windows_support.reason(&windows).is_none());
        } else {
            assert_eq!(windows_support, BackendSupport::WindowsBackendUnavailable);
            assert!(
                windows_support
                    .reason(&windows)
                    .unwrap()
                    .contains("backend unavailable")
            );
        }
    }

    #[test]
    fn runtime_template_names_match_the_frontend_contract() {
        assert_eq!(runtime_template_name(Frontend::Gui), "zup-setup-gui");
        assert_eq!(expected_subsystem(Frontend::Gui), PeSubsystem::Gui);
        for frontend in [Frontend::Console, Frontend::Headless] {
            assert_eq!(expected_subsystem(frontend), PeSubsystem::Console);
            assert!(
                validate_runtime_template(Path::new(runtime_template_name(frontend)), frontend)
                    .is_ok()
            );
        }
        assert!(
            validate_runtime_template(Path::new("zup-setup-headless"), Frontend::Console).is_err()
        );
        assert!(
            validate_runtime_template(Path::new("zup-setup"), Frontend::Gui).is_ok(),
            "the GUI alias stays a valid GUI template"
        );
    }

    #[test]
    fn preflight_collects_cardinality_problems_for_every_target() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let manifest = Path::new("/tmp/project/zup.toml");
        let inputs = inspect_build_inputs(Overwrite::Refuse, &[], &[], manifest, &app(), &targets);
        assert_eq!(inputs.runtimes.len(), 2);
        for slot in &inputs.runtimes {
            assert!(slot.path.is_none());
            assert_eq!(slot.problems.len(), 1);
            assert!(
                slot.problems[0].message.contains("multiple targets"),
                "{}",
                slot.problems[0].message
            );
        }
        let enforced = resolve_build_inputs(
            InputMode::Enforce,
            Overwrite::Refuse,
            &[],
            &[],
            manifest,
            &app(),
            &targets,
        );
        let error = enforced.unwrap_err().to_string();
        assert!(error.contains("multiple targets"), "{error}");

        let one = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("does-not-exist.exe");
        let short =
            inspect_build_inputs(Overwrite::Refuse, &[one], &[], manifest, &app(), &targets);
        for index in 0..2 {
            let problems = short.problems_for(index).collect::<Vec<_>>();
            assert!(!problems.is_empty(), "target {index} has no problem");
            assert_eq!(problems[0].subject, InputSubject::Runtime);
            assert_eq!(
                problems[0].message,
                "selected 2 targets but received 1 runtimes; provide one --runtime per target"
            );
        }
        assert_eq!(
            short.problems_for(0).count(),
            2,
            "the unresolved path is reported next to the cardinality problem"
        );
    }

    #[test]
    fn every_repeatable_per_target_flag_uses_one_alignment_message() {
        let empty: [&str; 0] = [];
        assert!(
            align_per_target("runtimes", "--runtime", &empty, 2)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            align_per_target("runtimes", "--runtime", &["a", "b"], 2)
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
            let error = align_per_target(noun, flag, &["one"], 2)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                format!("selected 2 targets but received 1 {noun}; provide one {flag} per target")
            );
            let surplus = align_per_target(noun, flag, &["one", "two", "three"], 2)
                .unwrap_err()
                .to_string();
            assert_eq!(
                surplus,
                format!("selected 2 targets but received 3 {noun}; provide one {flag} per target")
            );
        }
    }

    #[test]
    fn derived_output_names_are_distinct_per_target() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let manifest = Path::new("/tmp/project/zup.toml");
        let inputs = inspect_build_inputs(Overwrite::Refuse, &[], &[], manifest, &app(), &targets);
        let names = inputs
            .outputs
            .iter()
            .map(|slot| slot.path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "/tmp/project\\Doctor App-Setup-alpha.exe",
                "/tmp/project\\Doctor App-Setup-beta.exe"
            ]
        );
        assert!(inputs.outputs.iter().all(|slot| slot.derived));

        let single =
            inspect_build_inputs(Overwrite::Refuse, &[], &[], manifest, &app(), &targets[..1]);
        assert_eq!(
            single.outputs[0].path.display().to_string(),
            "/tmp/project\\Doctor App-Setup.exe"
        );
    }

    #[test]
    fn an_existing_output_is_refused_unless_it_may_be_overwritten() {
        let existing = tempfile::NamedTempFile::new().unwrap();
        let manifest = Path::new("/tmp/project/zup.toml");
        let output = existing.path().to_path_buf();
        let targets = [target_config("alpha")];

        let refused = inspect_build_inputs(
            Overwrite::Refuse,
            &[],
            std::slice::from_ref(&output),
            manifest,
            &app(),
            &targets,
        );
        let problem = refused.outputs[0]
            .problems
            .iter()
            .find(|problem| problem.message.contains("already exists"))
            .expect("an existing output is a problem");
        assert!(
            problem.message.contains("--force"),
            "the refusal names the escape hatch: {}",
            problem.message
        );

        let forced = inspect_build_inputs(
            Overwrite::Force,
            &[],
            std::slice::from_ref(&output),
            manifest,
            &app(),
            &targets,
        );
        assert!(
            forced.outputs[0].problems.is_empty(),
            "--force permits replacing an existing output: {:?}",
            forced.outputs[0].problems
        );
    }

    #[test]
    fn the_backend_boundary_needs_no_filesystem() {
        let mut linux = target_config("linux");
        linux.target = TargetTriple::parse("aarch64-unknown-linux-gnu").unwrap();
        let mut windows = target_config("windows");
        windows.target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();

        let error = check_backend_support(&linux).unwrap_err().to_string();
        assert!(error.contains("unsupported backend for target"), "{error}");

        match backend_support(&windows.target) {
            BackendSupport::Ready => assert!(check_backend_support(&windows).is_ok()),
            _ => assert!(
                check_backend_support(&windows).is_err(),
                "an unavailable backend is refused on every host"
            ),
        }
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
