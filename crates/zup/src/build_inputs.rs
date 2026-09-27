//! Shared build-input resolution for `zup build` and `zup doctor`.
//!
//! Both commands map every selected target profile to exactly one runtime
//! template and one output path. That mapping lives here once, so the readiness
//! report describes the inputs `zup build` will actually use instead of
//! re-deriving names and re-implementing the checks.
//!
//! The runtime template is resolved here, through the toolchain resolver, because
//! "which template goes with this target" and "is that template usable" are one
//! question. A readiness report that asked for a different answer from the build
//! it is reporting on would be a report about something else.
//!
//! [`InputMode::Enforce`] fails on the first problem so `zup build` writes
//! nothing. [`InputMode::Preflight`] records every problem against its target
//! so `zup doctor` can report all of them in one pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{App, ResolvedTargetConfig, SelectedScope, TargetOperatingSystem, TargetTriple};

use crate::toolchain::{ToolchainResolver, ToolchainSource};

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
    /// Where the resolved component came from, when one was resolved.
    pub source: Option<ToolchainSource>,
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

/// Resolve the runtime template for every selected target.
///
/// `Enforce` returns the first problem as an error, so `zup build` writes
/// nothing. `Preflight` returns every slot with its problems attached, so
/// `zup doctor` can report all of them in one pass.
///
/// Outputs are resolved separately because a caller that composes artifacts has
/// one output per *artifact* rather than one per target, and a per-target
/// alignment rule would be the wrong rule for one. The runtime slots are the same
/// either way, and are the part that decides whether a build can happen at all.
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

/// Resolve one output per selected target, with every problem attached to its own
/// slot.
///
/// `runtimes` is the already-resolved runtime list, because an output that is
/// also a selected template is a mistake a build would otherwise discover by
/// writing over its own input.
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

/// The first problem, runtimes before outputs.
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

/// One output per selected target, with every problem attached to its own slot.
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

/// The diagnostic for a repeatable per-target flag that does not line up with
/// the selected targets. One value per target, or none at all.
///
/// The order matters and is not obvious: it is the manifest's own target order,
/// so the message names the profiles rather than leaving the user to guess which
/// template goes with which target.
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

/// One runtime template per selected target, from an explicit path or the
/// toolchain resolver.
///
/// An explicit path is a per-target override, and it is checked exactly like a
/// resolved one: a component that was not produced by this zup release, or is
/// for another machine or frontend, is refused here rather than composed into an
/// installer.
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
    use zup_core::Frontend;

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

    /// A resolver that finds nothing, so a test is about the shape of a problem
    /// rather than about whichever toolchain the machine happens to have staged.
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
        // Neither slot got a component, so neither silently takes the other's, and
        // each says why: the component a build would have asked for is not on this
        // machine. An absent `--runtime` is the resolver's question, not a count,
        // so there is nothing else to report.
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

    /// One value for two targets names the profiles, and names the path it reached.
    #[test]
    fn a_short_runtime_list_names_the_profiles_it_was_given_against() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let one = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("one-component.exe");
        let slots = resolve_runtimes(
            InputMode::Preflight,
            &empty_resolver(),
            std::slice::from_ref(&one),
            &targets,
        )
        .expect("preflight reports rather than raises");
        for (index, slot) in slots.iter().enumerate() {
            assert_eq!(slot.problems.len(), 2, "target {index}: {slot:?}");
            assert_eq!(
                slot.problems[0].message,
                "selected 2 targets (alpha, beta) but received 1 runtimes; provide one --runtime per target, in that order"
            );
            // The path is only on the slot the path reached: the second target was
            // given nothing, so the message it gets is about the toolchain, not
            // about a file.
            if index == 0 {
                assert_eq!(slot.problems[1].path.as_deref(), Some(one.as_path()));
            } else {
                assert!(slot.problems[1].path.is_none());
            }
        }
    }

    /// An empty `--runtime` is not a count of zero. It is the toolchain resolver
    /// being asked, which is the ordinary way a build finds its templates.
    #[test]
    fn an_absent_runtime_flag_is_not_a_cardinality_problem() {
        let targets = vec![target_config("alpha")];
        let slots = resolve_runtimes(InputMode::Preflight, &empty_resolver(), &[], &targets)
            .expect("preflight reports rather than raises");
        assert!(
            slots[0]
                .problems
                .iter()
                .all(|problem| !problem.message.contains("--runtime per target")),
            "an absent flag is the resolver's question, not a count: {:?}",
            slots[0].problems
        );
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
    fn derived_output_names_are_distinct_per_target() {
        let targets = vec![target_config("alpha"), target_config("beta")];
        let manifest = Path::new("/tmp/project/zup.toml");
        let outputs = inspect_output_slots(Overwrite::Refuse, &[], manifest, &app(), &targets, &[]);
        let names = outputs
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
        assert!(outputs.iter().all(|slot| slot.derived));

        let single =
            inspect_output_slots(Overwrite::Refuse, &[], manifest, &app(), &targets[..1], &[]);
        assert_eq!(
            single[0].path.display().to_string(),
            "/tmp/project\\Doctor App-Setup.exe"
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
