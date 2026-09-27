//! The dispatch loop.
//!
//! This is the whole of a universal artifact's launcher. It:
//!
//! 1. inspects the host,
//! 2. parses and validates the artifact index,
//! 3. selects the best compatible variant,
//! 4. verifies and materializes that variant's native runtime and content,
//! 5. starts it,
//! 6. forwards its result.
//!
//! It does not touch the registry, create services, plan a lifecycle, elevate, or
//! install prerequisites. Every one of those is the selected native runtime's
//! job, in its own architecture, which is why this program can be small enough
//! to run under an emulation layer on a machine whose native variant is
//! something else.
//!
//! Nothing is guessed. The variant comes from the index, the content comes from
//! descriptors in that index, and every byte is verified against a digest before
//! it is written.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zup_windows::{ContentStoreIdentity, UniversalArtifact};

mod report;

/// The dispatcher's own outcome, which is what the launcher tells the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The selected variant was started and finished.
    Completed { code: i32 },
    /// No variant in this artifact can run on this host.
    Unsupported { detail: String },
    /// The artifact could not be read, and the reason is a defect rather than an
    /// unsupported host.
    Refused { detail: String },
}

/// Dispatch the artifact at `executable`, using `state_root` as the place a
/// user-scoped installation's state belongs.
///
/// `state_root` is `None` for the default, which is what a user double-clicking
/// the artifact gets.
pub fn dispatch(executable: &Path, state_root: Option<&Path>) -> Outcome {
    let artifact = match UniversalArtifact::open(executable) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };
    let index = artifact.index();
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
    let state_root = match state_root {
        Some(root) => root.to_path_buf(),
        None => match default_state_root(scope) {
            Ok(root) => root,
            Err(detail) => return Outcome::Refused { detail },
        },
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

    let arguments = [
        "install".to_owned(),
        "--state-root".to_owned(),
        state_root.display().to_string(),
        "--scope".to_owned(),
        match identity.scope() {
            zup_core::SelectedScope::User => "user".to_owned(),
            zup_core::SelectedScope::Machine => "machine".to_owned(),
        },
    ];
    match launch(&staged.runtime, &arguments) {
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

fn default_state_root(scope: zup_core::SelectedScope) -> Result<PathBuf, String> {
    let variable = match scope {
        zup_core::SelectedScope::User => "LOCALAPPDATA",
        zup_core::SelectedScope::Machine => "PROGRAMDATA",
    };
    let base = std::env::var_os(variable).ok_or_else(|| {
        format!("{variable} is not set, so there is nowhere to record the installation")
    })?;
    Ok(PathBuf::from(base).join("zup"))
}

/// Start the selected variant's native runtime and wait for it.
fn launch(runtime: &Path, arguments: &[String]) -> Result<i32, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new(runtime);
    command.args(arguments);
    // A windowed launcher must not leave a console behind, and a console one
    // must keep it: the subsystem of the launcher is what the user sees.
    command.creation_flags(CREATE_NO_WINDOW);
    let status = command
        .status()
        .map_err(|error| format!("starting the selected variant failed: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

/// The process entry point, shared by the windowed and console dispatchers.
pub fn main_with(executable: Option<PathBuf>, state_root: Option<PathBuf>) -> ExitCode {
    let executable = match executable.or_else(|| std::env::current_exe().ok()) {
        Some(executable) => executable,
        None => {
            report::error("the dispatcher's own path is unavailable");
            return ExitCode::from(70);
        }
    };
    let state_root = state_root.or_else(argument_state_root);
    match dispatch(&executable, state_root.as_deref()) {
        Outcome::Completed { code } => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)),
        Outcome::Unsupported { detail } => {
            report::error(&format!(
                "this installer does not support this computer: {detail}"
            ));
            ExitCode::from(9)
        }
        Outcome::Refused { detail } => {
            report::error(&format!("the installer could not start: {detail}"));
            ExitCode::from(70)
        }
    }
}

fn argument_state_root() -> Option<PathBuf> {
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--state-root"
            && let Some(value) = arguments.next()
        {
            return Some(PathBuf::from(value));
        }
    }
    None
}
