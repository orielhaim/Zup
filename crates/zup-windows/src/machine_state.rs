use std::path::{Path, PathBuf};

use zup_core::SelectedScope;

use crate::host_dirs::{self, HostDirError};

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

pub fn ensure_state_root(path: &Path) -> Result<PathBuf, MachineStateError> {
    std::fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    canonical(path)
}

pub fn default_state_root(scope: SelectedScope) -> Result<PathBuf, MachineStateError> {
    match scope {
        SelectedScope::User => {
            ensure_state_root(&host_dirs::user_data()?.join(zup_transaction::STATE_FOLDER))
        }
        SelectedScope::Machine => {
            let base = host_dirs::shared_data()?;
            canonical(&base).map(|base| base.join(zup_transaction::STATE_FOLDER))
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_dirs::{shared_data, user_data};
    use rstest::rstest;

    #[rstest]
    #[case(
        r"\\?\C:\Users\dev\AppData\Local\zup",
        r"C:\Users\dev\AppData\Local\zup"
    )]
    #[case(r"\\?\UNC\server\share\zup", r"\\server\share\zup")]
    #[case(r"C:\Program Files\Acme", r"C:\Program Files\Acme")]
    fn the_extended_length_prefix_is_not_part_of_an_installed_path(
        #[case] path: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(plain_path_text(Path::new(path)), expected);
    }

    #[test]
    #[ignore = "touches the real user profile and machine data root"]
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
}
