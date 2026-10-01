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

use zup_windows::{ContentStoreIdentity, UniversalArtifact};

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
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Completed { .. } => 0,
            Self::Unsupported { .. } => 9,
            Self::ResolveFailed { .. } => 10,
            Self::AcquisitionFailed { .. } => 11,
            Self::VerificationFailed { .. } => 12,
            Self::LaunchFailed { .. } => 13,
            Self::InstallerFailed { .. } => 1,
            Self::RebootRequired { .. } => 14,
            Self::RecoveryRequired { .. } => 15,
            Self::Refused { .. } => 70,
        }
    }
}

/// The arguments a launcher accepts.
///
/// Two, and both are configuration rather than authority: where the machine
/// keeps its state, and an optional local tree to satisfy a closure from. A
/// hostile value for either produces content that fails its digest check, never
/// content that passes one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub state_root: Option<PathBuf>,
    pub source: Option<PathBuf>,
    /// Report acquisition events on stdout as JSONL.
    pub machine_readable: bool,
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

    // A thin artifact has no bytes of its own beyond the index, so there is
    // nothing to select from locally. The release graph decides.
    //
    // The scope comes from the artifact's own trust block rather than from a
    // constant here. A thin artifact carries no variant manifest, so the scope the
    // application's plan declares has nowhere else to travel - and a launcher
    // that defaulted it would install a `machine`-scoped application into the
    // user's profile and call it a success.
    #[cfg(feature = "online")]
    if index.artifact.mode == zup_artifact::ArtifactMode::Thin {
        let Some(trust) = index.artifact.trust.as_ref() else {
            return Outcome::Refused {
                detail: "a thin artifact must carry a trust block".to_owned(),
            };
        };
        return online::run(
            index,
            &online::BootstrapRequest {
                source: options.source.clone(),
                state_root: options.state_root.clone(),
                machine_readable: options.machine_readable,
            },
            trust.scope.selected(),
            is_gui(),
        );
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
    let scope = scope_of(&manifest);
    let state_root = match state_root(options.state_root.as_deref(), scope) {
        Ok(root) => root,
        Err(detail) => return Outcome::Refused { detail },
    };
    let identity = ContentStoreIdentity::new(
        index.artifact.application.id.clone(),
        index.artifact.application.version.clone(),
        scope,
        manifest.target.clone(),
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

    let arguments = vec![
        "install".to_owned(),
        "--state-root".to_owned(),
        state_root.display().to_string(),
        "--scope".to_owned(),
        match identity.scope() {
            zup_core::SelectedScope::User => "user".to_owned(),
            zup_core::SelectedScope::Machine => "machine".to_owned(),
        },
    ];
    let handoff = if is_gui() {
        zup_windows::HandOff::Silent
    } else {
        zup_windows::HandOff::Console
    };
    match launch(&staged.runtime, &arguments, handoff) {
        Ok(code) => Outcome::Completed { code },
        Err(detail) => Outcome::Refused { detail },
    }
}

/// The scope the selected variant installs into.
fn scope_of(manifest: &zup_artifact::VariantManifest) -> zup_core::SelectedScope {
    match manifest.plan.installer.install.scope {
        zup_core::InstallScope::Machine => zup_core::SelectedScope::Machine,
        zup_core::InstallScope::User | zup_core::InstallScope::Either => {
            zup_core::SelectedScope::User
        }
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

/// The process entry point, shared by the windowed and console dispatchers.
pub fn main_with(executable: Option<PathBuf>, options: Options) -> ExitCode {
    let executable = match executable.or_else(|| std::env::current_exe().ok()) {
        Some(executable) => executable,
        None => {
            report::error("the dispatcher's own path is unavailable");
            return ExitCode::from(70);
        }
    };
    let outcome = dispatch(&executable, &options);
    match &outcome {
        Outcome::Completed { .. } => {}
        other => report::error(&other.describe()),
    }
    ExitCode::from(outcome.exit_code())
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
/// Deliberately hand-rolled and deliberately tiny: two flags, `--flag value` and
/// `--flag=value`, both checked by name. A launcher that pulled in a parser to
/// read two options would be larger than the options.
pub fn options_from(args: impl Iterator<Item = std::ffi::OsString>) -> (Options, Option<PathBuf>) {
    let mut options = Options::default();
    let mut state_root = None;
    let mut iter = args.peekable();
    while let Some(argument) = iter.next() {
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
        match name.as_str() {
            "--state-root" => state_root = value().map(PathBuf::from),
            "--source" => options.source = value().map(PathBuf::from),
            "--output" => {
                if value().as_deref() == Some("jsonl") {
                    options.machine_readable = true;
                }
            }
            "--jsonl" => options.machine_readable = true,
            "--help" | "-h" => {
                report::print_usage();
            }
            _ => {}
        }
    }
    (options, state_root)
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
            options_from(args(&["--state-root", r"C:\s", "--source", r"X:\M"]).into_iter());
        assert_eq!(state_root, Some(PathBuf::from(r"C:\s")));
        assert_eq!(options.source, Some(PathBuf::from(r"X:\M")));
        assert!(!options.machine_readable);

        let (options, state_root) = options_from(
            args(&[r"--state-root=C:\s", r"--source=X:\M", "--output", "jsonl"]).into_iter(),
        );
        assert_eq!(state_root, Some(PathBuf::from(r"C:\s")));
        assert_eq!(options.source, Some(PathBuf::from(r"X:\M")));
        assert!(options.machine_readable);
    }

    #[test]
    fn an_unrecognised_argument_is_ignored_rather_than_fatal() {
        // A launcher is started by a shell, a shortcut, or a service manager, and
        // none of them know its flags. Refusing an argument it does not
        // recognise would make it fragile for no security gain: every value it
        // does read is a location, and a location cannot grant trust.
        let (options, state_root) = options_from(args(&["--ui", "--nonsense"]).into_iter());
        assert!(options.source.is_none());
        assert!(state_root.is_none());
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
