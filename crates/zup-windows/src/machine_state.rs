//! Windows installed-machine policy: where state lives and what it is called.
//!
//! These are decisions about *this machine*, not about an installation plan. The
//! state root a scope owns, the shape a path is written to disk as, and the name
//! the persisted maintenance executable is given all belong to the Windows
//! backend, because a different backend answers them differently and because the
//! developer CLI has no business implementing any of them. Which directory a
//! scope's state sits *in* is `host_dirs`' question, not this one's.

use std::path::{Path, PathBuf};

use zup_core::{AppId, SelectedScope};

use crate::host_dirs::{self, HostDirError};

/// The directory name every zup state root ends in.
const STATE_FOLDER: &str = "zup";

/// Failures produced while resolving machine state.
#[derive(Debug, thiserror::Error)]
pub enum MachineStateError {
    #[error(transparent)]
    HostDir(#[from] HostDirError),
    #[error("state root `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Create `path` if it is missing and return it in its canonical form.
///
/// Canonical rather than merely absolute because everything downstream - the
/// ledger, the transaction store, the installation lock - treats a state root as
/// an identity, and two spellings of one directory are two identities to them.
pub fn ensure_state_root(path: &Path) -> Result<PathBuf, MachineStateError> {
    std::fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    canonical(path)
}

/// The state root a scope owns when the caller names none.
///
/// A user install owns `<user data>/zup`; a machine install owns
/// `<shared data>/zup`. The machine root is only canonicalized, never created:
/// writing into the machine-wide data root is the elevated worker's job, and a
/// check that runs before elevation has no business making the directory.
pub fn default_state_root(scope: SelectedScope) -> Result<PathBuf, MachineStateError> {
    match scope {
        SelectedScope::User => ensure_state_root(&host_dirs::user_data()?.join(STATE_FOLDER)),
        SelectedScope::Machine => {
            let base = host_dirs::shared_data()?;
            canonical(&base).map(|base| base.join(STATE_FOLDER))
        }
    }
}

/// The state root an invocation should use.
///
/// An explicit machine-scope root is accepted before it exists, because a
/// machine-scope install frequently creates the state root as part of its first
/// transaction. Everything else is created and canonicalized.
pub fn resolve_state_root(
    explicit: Option<PathBuf>,
    scope: SelectedScope,
) -> Result<PathBuf, MachineStateError> {
    match explicit {
        Some(path) if scope == SelectedScope::Machine => {
            if path.exists() || path.is_absolute() {
                if path.exists() {
                    ensure_state_root(&path)
                } else {
                    Ok(path)
                }
            } else {
                std::env::current_dir()
                    .map_err(|source| io_error(&path, source))
                    .map(|directory| directory.join(path))
            }
        }
        Some(path) => ensure_state_root(&path),
        None => default_state_root(scope),
    }
}

fn canonical(path: &Path) -> Result<PathBuf, MachineStateError> {
    path.canonicalize().map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> MachineStateError {
    MachineStateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A path as one line of text, with the extended-length prefix removed.
///
/// Windows APIs hand back `\\?\`-prefixed paths once a component is long. That
/// prefix is an implementation detail of the call, not of the installation, so
/// it is stripped before a path is compared with a manifest template, written
/// into a ledger, or shown to a person.
pub fn plain_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        text.into_owned()
    }
}

/// Whether this process is the maintenance copy an installation persisted.
///
/// The maintenance copy is recognized by where it lives rather than by what it
/// is called, so the name stays free to change and an installer the user renamed
/// is still recognized for what it is.
pub fn is_maintenance_executable(path: &Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\maintenance\\")
}

/// Where an installation persists the maintenance runtime for one scope.
///
/// Versioned, so an upgrade stages the new copy beside the old one and the
/// transaction engine can roll back to bytes that are still on disk. The file
/// name is deliberately not `Setup.exe`: the user-facing installer a person
/// downloads is an installation medium, and the executable that persists beside
/// an installed application is a maintenance runtime, and calling the second one
/// the first is how a support conversation starts in the wrong place.
pub fn maintenance_destination(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &str,
) -> PathBuf {
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    state_root
        .join("maintenance")
        .join(app_id.as_str())
        .join(scope_name)
        .join(version)
        .join(crate::MAINTENANCE_EXECUTABLE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_dirs::{shared_data, user_data};

    #[test]
    fn the_extended_length_prefix_is_not_part_of_an_installed_path() {
        assert_eq!(
            plain_path_text(Path::new(r"\\?\C:\Users\dev\AppData\Local\zup")),
            r"C:\Users\dev\AppData\Local\zup"
        );
        assert_eq!(
            plain_path_text(Path::new(r"\\?\UNC\server\share\zup")),
            r"\\server\share\zup"
        );
        assert_eq!(
            plain_path_text(Path::new(r"C:\Program Files\Acme")),
            r"C:\Program Files\Acme"
        );
    }

    /// The scopes differ in more than which base directory they sit in: the user
    /// root is created on demand, and the machine root is only read.
    #[test]
    fn the_default_state_root_depends_on_the_scope_and_never_on_the_environment() {
        let user = default_state_root(SelectedScope::User).expect("a user state root");
        assert_eq!(
            user,
            user_data()
                .expect("a user data directory")
                .canonicalize()
                .expect("the user data root exists")
                .join("zup"),
            "a user install's state lives in the user's own data directory"
        );
        assert!(
            user.is_dir(),
            "a user state root is created: {}",
            user.display()
        );

        let machine = default_state_root(SelectedScope::Machine).expect("a machine state root");
        assert_eq!(
            machine,
            shared_data()
                .expect("a shared data directory")
                .canonicalize()
                .expect("the shared data root exists")
                .join("zup")
        );
        assert!(
            !machine.exists(),
            "creating the machine root is the elevated worker's job, not a check's"
        );
    }

    #[test]
    fn the_maintenance_copy_is_recognized_by_where_it_lives() {
        let root = Path::new(r"C:\Users\dev\AppData\Local\zup");
        assert!(is_maintenance_executable(&maintenance_destination(
            root,
            &AppId::new("com.example.acme").expect("valid"),
            SelectedScope::User,
            "1.2.0",
        )));
        assert!(!is_maintenance_executable(Path::new(
            r"C:\Users\dev\Downloads\Acme-Setup.exe"
        )));
    }
}
