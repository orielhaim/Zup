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
/// slot.///
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
///
/// Presets have no slot here. A GUI target's preset is selected and proved while
/// the project is materialized, because a preset's settings name project files
/// that have to be resolved and hashed in the same pass as the rest of the
/// payload; a slot that only held a toolchain path could not have done that.
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

/// The file name a derived output is given.
///
/// The suffix is the *target's*, not the build host's. A derived name that carried
/// the host's suffix would name a file no composition for that target writes, and
/// the error would surface much later - when something tried to run it.
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
        // A disambiguating number belongs before the suffix, or the second
        // candidate is a different file from the first rather than the same one.
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

/// The default target triple for a new manifest on this build host.
///
/// The host's own platform, not a fixed triple: a generated project should
/// build where it was created. A Linux host gets the one Linux target this
/// Zup version builds; any other host keeps the Windows default it always had.
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

/// Backend support for one target on this build host.
///
/// The three answers are the portable selection model's own: a host that can
/// lower this target natively, a host that has the target's backend but is not
/// the platform it belongs to, and a target no implemented backend answers for.
///
/// Nothing here names Windows. A vocabulary whose only members are `Windows...`
/// is a vocabulary that will be wrong the moment the second backend lands, and the
/// failure it produces - "WindowsBackendUnavailable" on a Linux host - reads as
/// though Windows were the universal backend rather than the one that happens to
/// be implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSupport {
    /// The host's backend can lower this target here.
    Ready,
    /// The target's backend is implemented, but not on this build host.
    ///
    /// A Windows target on a Linux build host, or the reverse. The backend exists;
    /// the *host* cannot run it, so the answer is about this machine rather than
    /// about the target.
    HostBackendUnavailable,
    /// A target no implemented backend answers for.
    NotImplemented,
}

impl BackendSupport {
    /// Why this host cannot build the target, or `None` when it can.
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

    /// The same boundary as a hard `zup build` error.
    pub fn build_error(self, target: &TargetTriple) -> Option<String> {
        self.reason(target)
            .map(|reason| format!("unsupported backend for target `{target}`: {reason}"))
    }
}

/// Classify one target against the backends this host implements.
///
/// Two questions, in this order, because they have different answers and the
/// second is the one that used to be conflated with "no backend exists": does an
/// implemented backend own this target's platform at all, and can this build host
/// run it.
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

/// Whether this build host can lower a target for `os`.
///
/// Composing an installer is bytes, not execution: any host composes a Linux
/// installer out of a runtime template and a package without running either,
/// so Linux lowers everywhere its templates resolve. Windows composition runs
/// Windows-only machinery, so a Windows target still needs a Windows host.
/// Host-specific behavior stays here; everything target-specific lives in the
/// capability checks behind [`check_target_lowering`].
const fn host_can_lower(os: TargetOperatingSystem) -> bool {
    matches!(os, TargetOperatingSystem::Linux)
        || (matches!(os, TargetOperatingSystem::Windows) && cfg!(windows))
}

/// The portable name of a target platform, for a diagnostic that has to say which.
///
/// `target_lexicon`'s `Display` is the triple spelling - `windows`, `linux` - which
/// is already the name a person reading the diagnostic would use, so the mapping
/// is only needed to normalise the ones whose `Debug` is not lower case.
fn platform_name(os: TargetOperatingSystem) -> String {
    // `target_lexicon` spells an operating system through its own `Display`, which
    // is already the lower-case triple spelling a diagnostic would use, and the two
    // names this backend distinguishes are spelled out because a diagnostic should
    // not depend on a foreign type's `Display` staying as it is.
    match os {
        TargetOperatingSystem::Windows => "windows".to_owned(),
        TargetOperatingSystem::Linux => "linux".to_owned(),
        other => other.to_string(),
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

/// Whether target lowering resolves for every install scope of a target.
///
/// The dispatch is on the *target's* operating system, never on the build
/// host's: a Windows host building a Linux target takes the Linux arm, and a
/// Linux host asked about a Windows target takes the Windows arm. Host
/// behavior (which template bytes, which syscalls) stays behind the
/// composition each arm calls; the decision of which arm answers is purely
/// about what is being built.
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

/// Prove a Linux target lowers, on any build host.
///
/// The capability check reads the portable installer IR and the materialized
/// plan, so a Windows host answers the same question a Linux host answers. On
/// a Linux host the portable answer is then confirmed by the real thing: the
/// semantic plan lowers through the Linux backend exactly as an install would.
/// A cross host cannot run that confirmation, and does not need to: nothing it
/// confirms is about the bytes being composed.
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
    #[cfg(target_os = "linux")]
    {
        let request = zup_plan::PlanRequest::new(config.target.clone(), SelectedScope::User);
        let install = zup_plan::plan_without_plugins(build, &request).map_err(|error| {
            miette::miette!("semantic plan for target `{}`: {error}", config.target)
        })?;
        zup_linux::resolve_target(&install).map_err(|error| {
            miette::miette!("Linux lowering for target `{}`: {error}", config.target)
        })?;
    }
    Ok(vec![SelectedScope::User])
}

/// Whether Windows target lowering resolves for every install scope of a target.
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

        // Linux is a platform an implemented backend owns on every build host:
        // composing a Linux installer is bytes, not execution, so any host that
        // resolves a runtime template lowers the target.
        let linux_support = backend_support(&linux);
        assert_eq!(linux_support, BackendSupport::Ready);
        assert!(
            linux_support.reason(&linux).is_none(),
            "a ready backend has no reason"
        );
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

        // One target is one output, and the name is the whole of what a build
        // composes; nothing picks a directory for it.
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
