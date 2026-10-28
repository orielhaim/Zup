#![cfg(windows)]
//! The dispatch loop.
//!
//! This is the whole of a universal artifact's launcher. It:
//!
//! 1. inspects the host,
//! 2. parses and validates the artifact index,
//! 3. either materializes the selected variant from the artifact's own bytes
//!    (an **offline** artifact) or resolves a release graph and fetches the
//!    variant's native runtime (a **thin** artifact),
//! 4. verifies and starts that runtime,
//! 5. forwards its result.
//!
//! It does not touch the registry, create services, plan a lifecycle, elevate, or
//! install prerequisites. Every one of those is the selected native runtime's
//! job, in its own architecture, which is why this program can be small enough
//! to run under an emulation layer on a machine whose native variant is
//! something else.
//!
//! Nothing is guessed. The variant comes from the index, the content comes from
//! descriptors in that index or in an authenticated release, and every byte is
//! verified against a digest before it is written or run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zup_transaction::ContentStoreIdentity;
use zup_windows::UniversalArtifact;

mod report;

#[cfg(feature = "online")]
mod events;
#[cfg(feature = "online")]
mod online;

/// The dispatcher's own outcome, which is what the launcher tells the user.
///
/// The variants are separate because the answers are separate. "This installer
/// does not support this computer", "this installer could not find a release it
/// trusts", "the download failed", and "the installer ran and failed" are four
/// problems, and a script or a support engineer needs to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The selected variant was started and finished.
    Completed { code: i32 },
    /// No variant in this artifact can run on this host.
    Unsupported { detail: String },
    /// A thin artifact could not authenticate a release.
    ResolveFailed { detail: String },
    /// A thin artifact could not acquire the content it needed.
    AcquisitionFailed { detail: String },
    /// Content was acquired but did not match what was authenticated.
    VerificationFailed { detail: String },
    /// The verified runtime could not be started.
    LaunchFailed { detail: String },
    /// The native installer ran and reported this code.
    InstallerFailed { code: i32 },
    /// The installation needs a restart before it can finish.
    RebootRequired { code: i32 },
    /// A previous transaction must be recovered first.
    RecoveryRequired { code: i32 },
    /// The artifact itself could not be read, and the reason is a defect.
    Refused { detail: String },
}

impl Outcome {
    /// The process exit code for this outcome.
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Completed { code }
            | Self::InstallerFailed { code }
            | Self::RebootRequired { code }
            | Self::RecoveryRequired { code } => *code,
            Self::Unsupported { .. } => 9,
            Self::ResolveFailed { .. } => 10,
            Self::AcquisitionFailed { .. } => 11,
            Self::VerificationFailed { .. } => 12,
            Self::LaunchFailed { .. } => 13,
            Self::Refused { .. } => 70,
        }
    }
}

/// The arguments a launcher accepts.
///
/// A lifecycle verb plus configuration. Everything here is configuration
/// rather than authority: where the machine keeps its state, which lifecycle
/// step to run, and how to report. A hostile value for any of them produces
/// content that fails its digest check, never content that passes one. Values
/// the staged runtime needs travel verbatim; the runtime re-validates every
/// one of them before acting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub state_root: Option<PathBuf>,
    pub source: Option<PathBuf>,
    /// Report acquisition events on stdout as JSONL.
    pub machine_readable: bool,
    pub verb: Verb,
    pub scope: Option<zup_core::SelectedScope>,
    pub non_interactive: bool,
    pub yes: bool,
    pub force_files: bool,
    pub plain: bool,
    pub output: Option<String>,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
    pub update_check: bool,
    pub help: bool,
}

/// The operation forwarded to the native runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Verb {
    #[default]
    Install,
    Upgrade,
    Modify,
    Repair,
    Update,
    Uninstall,
}

impl Verb {
    fn runtime_name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Modify => "modify",
            Self::Repair => "repair",
            Self::Update => "update",
            Self::Uninstall => "uninstall",
        }
    }

    fn from_arg(arg: &str) -> Option<Self> {
        match arg.to_ascii_lowercase().as_str() {
            "install" => Some(Self::Install),
            "upgrade" => Some(Self::Upgrade),
            "modify" => Some(Self::Modify),
            "repair" => Some(Self::Repair),
            "update" => Some(Self::Update),
            "uninstall" => Some(Self::Uninstall),
            _ => None,
        }
    }
}

/// Dispatch the artifact at `executable`, using `options` for its state.
pub fn dispatch(executable: &Path, options: &Options) -> Outcome {
    let artifact = match UniversalArtifact::open(executable) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };
    let index = artifact.index();
    if let Err(detail) = index.artifact.validate() {
        return Outcome::Refused {
            detail: detail.to_owned(),
        };
    }

    #[cfg(feature = "online")]
    if index.artifact.mode == zup_artifact::ArtifactMode::Thin {
        let Some(trust) = index.artifact.trust.as_ref() else {
            return Outcome::Refused {
                detail: "a thin artifact must carry a trust block".to_owned(),
            };
        };
        let scope = match select_scope(trust.scope, options) {
            Ok(scope) => scope,
            Err(detail) => return Outcome::Refused { detail },
        };
        return online::run(index, options, scope, is_gui());
    }
    #[cfg(not(feature = "online"))]
    if index.artifact.mode == zup_artifact::ArtifactMode::Thin {
        return Outcome::Refused {
            detail: "this build of the installer cannot fetch a release".to_owned(),
        };
    }

    let selection = match artifact.select() {
        Ok(selection) => selection,
        Err(error) if error.is_unsupported_host() => {
            return Outcome::Unsupported {
                detail: error.to_string(),
            };
        }
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };

    report::describe(&selection, index);

    let manifest = match artifact.view().verify_variant(&selection.id) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };
    let scope = match select_scope(manifest.plan.installer.install.scope, options) {
        Ok(scope) => scope,
        Err(detail) => {
            return Outcome::Refused { detail };
        }
    };
    let state_root = match state_root(options.state_root.as_deref(), scope) {
        Ok(root) => root,
        Err(detail) => return Outcome::Refused { detail },
    };
    let identity = ContentStoreIdentity::new(
        index.artifact.application.id.clone(),
        index.artifact.application.version.clone(),
        scope,
        manifest.target,
        artifact_digest(&artifact),
    );
    let base = match zup_windows::content_store_base(&state_root, identity.scope()) {
        Ok(base) => base,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };
    let store = identity.path_under(&base);
    if let Err(error) = zup_windows::ensure_directory(&store) {
        return Outcome::Refused {
            detail: error.to_string(),
        };
    }
    if let Err(error) = zup_windows::verify_directory_chain(&base, &store) {
        return Outcome::Refused {
            detail: error.to_string(),
        };
    }
    let staged = match zup_windows::stage_variant(&artifact, &selection.id, &store) {
        Ok(staged) => staged,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };

    let arguments = staged_arguments(options, &state_root, scope);
    let handoff = if is_gui() {
        zup_windows::HandOff::Silent
    } else {
        zup_windows::HandOff::Console
    };
    match launch(&staged.runtime, &arguments, handoff) {
        Ok(code) => classify(code),
        Err(detail) => Outcome::LaunchFailed { detail },
    }
}

/// The arguments the staged runtime runs with.
///
/// The verb and every flag travel verbatim; the runtime parses and validates
/// them exactly as if it had been started directly, so a dispatcher never
/// accepts something the runtime would refuse.
fn staged_arguments(
    options: &Options,
    state_root: &Path,
    scope: zup_core::SelectedScope,
) -> Vec<String> {
    let mut arguments = vec![options.verb.runtime_name().to_owned()];
    if options.verb == Verb::Update && options.update_check {
        arguments.push("check".to_owned());
    }
    arguments.push("--state-root".to_owned());
    arguments.push(state_root.display().to_string());
    arguments.push("--scope".to_owned());
    arguments.push(scope.to_string());
    if options.non_interactive {
        arguments.push("--non-interactive".to_owned());
    }
    if options.yes {
        arguments.push("--yes".to_owned());
    }
    if options.plain {
        arguments.push("--plain".to_owned());
    }
    if let Some(output) = &options.output {
        arguments.push("--output".to_owned());
        arguments.push(output.clone());
    }
    match options.verb {
        Verb::Install | Verb::Upgrade | Verb::Modify | Verb::Repair => {
            for id in &options.enable {
                arguments.push("--enable".to_owned());
                arguments.push(id.clone());
            }
            for id in &options.disable {
                arguments.push("--disable".to_owned());
                arguments.push(id.clone());
            }
            if let Some(directory) = &options.install_directory {
                arguments.push("--install-directory".to_owned());
                arguments.push(directory.display().to_string());
            }
            if options.verb == Verb::Repair && options.force_files {
                arguments.push("--force-files".to_owned());
            }
        }
        Verb::Update | Verb::Uninstall => {}
    }
    arguments
}

/// Preserve native runtime outcomes at the bootstrapper boundary.
pub fn classify(code: i32) -> Outcome {
    match code {
        0 => Outcome::Completed { code: 0 },
        3010 => Outcome::RebootRequired { code },
        7 => Outcome::RecoveryRequired { code },
        _ => Outcome::InstallerFailed { code },
    }
}

fn select_scope(
    declared: zup_core::InstallScope,
    options: &Options,
) -> Result<zup_core::SelectedScope, String> {
    match declared.select(options.scope) {
        Ok(scope) => Ok(scope),
        Err(zup_core::ScopeSelectionError::ChoiceRequired)
            if !options.non_interactive
                && !options.yes
                && !options.machine_readable
                && options.output.is_none() =>
        {
            report::choose_scope(is_gui())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The digest that makes a content store belong to exactly one artifact.
///
/// The table descriptor is the artifact's own content identity, so two releases
/// of the same application never share a store even when their versions differ.
fn artifact_digest(artifact: &UniversalArtifact) -> zup_core::Sha256Digest {
    artifact.view().index().tables.blobs.digest
}

/// The state root this dispatch writes into.
///
/// A launcher has no idea what the machine calls its directories, and asking it
/// was three copies of the same environment-variable probe that could disagree
/// with the runtime it starts. Both paths are the backend's question now.
pub(crate) fn state_root(
    named: Option<&Path>,
    scope: zup_core::SelectedScope,
) -> Result<PathBuf, String> {
    match named {
        Some(root) => Ok(root.to_path_buf()),
        None => zup_windows::default_state_root(scope)
            .map_err(|error| format!("{error}, so there is nowhere to record the installation")),
    }
}

/// Start the selected variant's native runtime and wait for it.
fn launch(
    runtime: &Path,
    arguments: &[String],
    handoff: zup_windows::HandOff,
) -> Result<i32, String> {
    let child = zup_windows::launch(runtime, arguments, handoff, None)
        .map_err(|error| format!("starting the selected variant failed: {error}"))?;
    Ok(child.wait())
}

/// Whether this launcher was built for a window.
///
/// The artifact's subsystem decides which template was composed, and this
/// process's subsystem is that template's, so the two agree by construction. It
/// matters because a GUI handoff must not briefly show two windows and a console
/// handoff must keep its terminal.
fn is_gui() -> bool {
    // A launcher with no console is a window. Reading our own image is the same
    // check `compose_universal_executable` makes when it refuses a mismatched
    // template, so a launcher cannot be composed into a subsystem it does not
    // report.
    matches!(own_program(), Ok(Some(zup_binary::ProgramKind::Windowed)))
}

fn own_program() -> Result<Option<zup_binary::ProgramKind>, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("the launcher's own path is unavailable: {error}"))?;
    zup_binary::Executable::read(&executable)
        .map(|executable| executable.program())
        .map_err(|error| error.to_string())
}

/// Report a command-line usage error.
///
/// Printed and, on Windows, shown, like every other dispatcher failure: a
/// windowed launcher has no terminal to read stderr from.
pub fn usage_error(error: &str) -> ExitCode {
    report::error(error, is_gui());
    report::print_usage();
    ExitCode::from(2)
}

/// The process entry point, shared by the windowed and console dispatchers.
pub fn main_with(executable: Option<PathBuf>, options: Options) -> ExitCode {
    if options.help {
        report::print_usage();
        return ExitCode::SUCCESS;
    }
    let executable = match executable.or_else(|| std::env::current_exe().ok()) {
        Some(executable) => executable,
        None => {
            report::error(
                "the dispatcher's own path is unavailable",
                is_gui() && !options.non_interactive,
            );
            return ExitCode::from(70);
        }
    };
    let outcome = dispatch(&executable, &options);
    match &outcome {
        Outcome::Completed { .. } => {}
        other => report::error(
            &other.describe(),
            is_gui()
                && !options.non_interactive
                && !options.yes
                && !options.machine_readable
                && options.output.is_none(),
        ),
    }
    let code = outcome.exit_code();
    if !(0..=255).contains(&code) {
        std::process::exit(code);
    }
    ExitCode::from(code as u8)
}

impl Outcome {
    /// A one-line description a person can act on.
    pub fn describe(&self) -> String {
        match self {
            Self::Completed { .. } => "installed".to_owned(),
            Self::Unsupported { detail } => {
                format!("this installer does not support this computer: {detail}")
            }
            Self::ResolveFailed { detail } => {
                format!("this installer could not find a release it trusts: {detail}")
            }
            Self::AcquisitionFailed { detail } => {
                format!("downloading the installer failed: {detail}")
            }
            Self::VerificationFailed { detail } => {
                format!("the downloaded installer did not match the release: {detail}")
            }
            Self::LaunchFailed { detail } => {
                format!("the verified installer could not be started: {detail}")
            }
            Self::InstallerFailed { code } => {
                format!("the installer could not complete (exit code {code})")
            }
            Self::RebootRequired { code } => {
                format!("the installation needs a restart before it can finish (exit code {code})")
            }
            Self::RecoveryRequired { code } => {
                format!("a previous installation must be recovered first (exit code {code})")
            }
            Self::Refused { detail } => format!("the installer could not start: {detail}"),
        }
    }
}

/// Read the launcher's own arguments.
///
/// Deliberately hand-rolled and deliberately tiny: a leading verb, `--flag
/// value` and `--flag=value`, both checked by name. A launcher that pulled in
/// a parser to read these options would be larger than the options. Only the
/// flags below travel to the staged runtime; anything else is refused rather
/// than silently dropped, so a mistyped invocation can never run an install
/// the user did not ask for.
pub fn options_from(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(Options, Option<PathBuf>), String> {
    let mut options = Options::default();
    let mut state_root = None;
    let mut verb_seen = false;
    let mut error = None;
    let mut iter = args.peekable();
    while let Some(argument) = iter.next() {
        if error.is_some() {
            break;
        }
        let text = argument.to_string_lossy().into_owned();
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (text, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| iter.next().map(|v| v.to_string_lossy().into_owned()))
        };
        let mut flag = |present: &mut bool| {
            if inline.is_some() {
                error = Some(format!("{name} takes no value"));
            } else {
                *present = true;
            }
        };
        match name.as_str() {
            "--state-root" => match value() {
                Some(root) => state_root = Some(PathBuf::from(root)),
                None => error = Some("--state-root needs a path".to_owned()),
            },
            "--source" => match value() {
                Some(source) => options.source = Some(PathBuf::from(source)),
                None => error = Some("--source needs a path".to_owned()),
            },
            "--output" => match value().as_deref() {
                Some(output @ ("human" | "json" | "jsonl")) => {
                    if output == "jsonl" {
                        options.machine_readable = true;
                    }
                    options.output = Some(output.to_owned());
                }
                _ => error = Some("--output needs human, json, or jsonl".to_owned()),
            },
            "--jsonl" => {
                if inline.is_some() {
                    error = Some("--jsonl takes no value".to_owned());
                } else {
                    options.machine_readable = true;
                    options.output = Some("jsonl".to_owned());
                }
            }
            "--scope" => match value().as_deref() {
                Some("user") => options.scope = Some(zup_core::SelectedScope::User),
                Some("machine") => options.scope = Some(zup_core::SelectedScope::Machine),
                _ => error = Some("--scope needs user or machine".to_owned()),
            },
            "--non-interactive" => flag(&mut options.non_interactive),
            "--yes" => flag(&mut options.yes),
            "--plain" => flag(&mut options.plain),
            "--force-files" => flag(&mut options.force_files),
            "--enable" | "--component" => match value() {
                Some(id) => options.enable.push(id),
                None => error = Some(format!("{name} needs a component id")),
            },
            "--disable" => match value() {
                Some(id) => options.disable.push(id),
                None => error = Some(format!("{name} needs a component id")),
            },
            "--install-directory" | "--install-dir" => match value() {
                Some(directory) => options.install_directory = Some(PathBuf::from(directory)),
                None => error = Some(format!("{name} needs a path")),
            },
            "--help" | "-h" => options.help = true,
            _ if name.starts_with("--") => {
                error = Some(format!("unrecognised option {name}"));
            }
            _ if !verb_seen => {
                verb_seen = true;
                match Verb::from_arg(&name) {
                    Some(verb) => options.verb = verb,
                    None => {
                        error = Some(format!(
                            "unrecognised verb `{name}`; expected install, modify, repair, \
                             upgrade, update, or uninstall"
                        ));
                    }
                }
            }
            _ if options.verb == Verb::Update && name == "check" => {
                options.update_check = true;
            }
            _ => {
                error = Some(format!("unexpected argument `{name}`"));
            }
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok((options, state_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn a_launcher_reads_two_options_in_either_spelling() {
        let (options, state_root) =
            options_from(args(&["--state-root", r"C:\s", "--source", r"X:\M"]).into_iter())
                .expect("known options parse");
        assert_eq!(state_root, Some(PathBuf::from(r"C:\s")));
        assert_eq!(options.source, Some(PathBuf::from(r"X:\M")));
        assert!(!options.machine_readable);

        let (options, state_root) = options_from(
            args(&[r"--state-root=C:\s", r"--source=X:\M", "--output", "jsonl"]).into_iter(),
        )
        .expect("known options parse");
        assert_eq!(state_root, Some(PathBuf::from(r"C:\s")));
        assert_eq!(options.source, Some(PathBuf::from(r"X:\M")));
        assert!(options.machine_readable);
    }

    #[test]
    fn verbs_and_lifecycle_flags_travel_to_the_staged_runtime() {
        let (options, _) = options_from(
            args(&[
                "repair",
                "--scope",
                "machine",
                "--force-files",
                "--yes",
                "--enable",
                "core",
                "--disable=extras",
                "--install-directory",
                r"C:\Apps\Acme",
            ])
            .into_iter(),
        )
        .expect("a verb with flags parses");
        assert_eq!(options.verb, Verb::Repair);
        assert_eq!(options.scope, Some(zup_core::SelectedScope::Machine));
        assert!(options.force_files);
        assert!(options.yes);
        assert_eq!(options.enable, vec!["core".to_owned()]);
        assert_eq!(options.disable, vec!["extras".to_owned()]);
        assert_eq!(
            options.install_directory,
            Some(PathBuf::from(r"C:\Apps\Acme"))
        );

        let (options, _) = options_from(args(&["uninstall", "--non-interactive"]).into_iter())
            .expect("uninstall parses");
        assert_eq!(options.verb, Verb::Uninstall);
        assert!(options.non_interactive);

        let (options, _) =
            options_from(args(&["upgrade", "--plain"]).into_iter()).expect("upgrade parses");
        assert_eq!(options.verb, Verb::Upgrade);
        let forwarded =
            staged_arguments(&options, Path::new("state"), zup_core::SelectedScope::User);
        assert_eq!(forwarded[0], "upgrade");
        assert!(forwarded.iter().any(|argument| argument == "--plain"));

        let (options, _) = options_from(args(&[]).into_iter()).expect("no verb parses");
        assert_eq!(options.verb, Verb::Install);
    }

    #[test]
    fn an_unrecognised_verb_or_option_is_fatal_rather_than_an_install() {
        // A mistyped invocation must never run an install the user did not ask
        // for. Every value the launcher forwards is re-validated by the staged
        // runtime, but the verb itself is the launcher's decision alone.
        for argv in [
            vec!["--ui"],
            vec!["--nonsense"],
            vec!["remove"],
            vec!["install", "--bogus"],
            vec!["--scope"],
            vec!["--scope", "everyone"],
            vec!["--output", "xml"],
            vec!["--state-root"],
            vec!["repair", "--enable"],
        ] {
            assert!(
                options_from(args(&argv).into_iter()).is_err(),
                "{argv:?} must not parse"
            );
        }
    }

    #[test]
    fn a_failed_runtime_is_never_reported_as_success() {
        assert_eq!(classify(0), Outcome::Completed { code: 0 });
        assert_eq!(classify(1), Outcome::InstallerFailed { code: 1 });
        assert_eq!(classify(7), Outcome::RecoveryRequired { code: 7 });
        assert_eq!(classify(3010), Outcome::RebootRequired { code: 3010 });
        assert_eq!(classify(255), Outcome::InstallerFailed { code: 255 });
        for code in [-1, 1, 2, 3, 5, 7, 255, 3010] {
            assert_eq!(classify(code).exit_code(), code);
        }
    }

    #[test]
    fn the_outcome_taxonomy_has_distinct_exit_codes() {
        let outcomes = [
            Outcome::ResolveFailed {
                detail: String::new(),
            },
            Outcome::Unsupported {
                detail: String::new(),
            },
            Outcome::AcquisitionFailed {
                detail: String::new(),
            },
            Outcome::VerificationFailed {
                detail: String::new(),
            },
            Outcome::LaunchFailed {
                detail: String::new(),
            },
            Outcome::InstallerFailed { code: 1 },
            Outcome::RebootRequired { code: 3010 },
            Outcome::RecoveryRequired { code: 7 },
        ];
        let mut codes: Vec<u8> = outcomes.iter().map(Outcome::exit_code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), outcomes.len(), "{outcomes:?}");
    }
}
