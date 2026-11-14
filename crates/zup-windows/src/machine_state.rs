use std::path::{Path, PathBuf};

use zup_core::SelectedScope;

use crate::host_dirs;

use crate::filesystem::FilesystemError;

#[derive(Clone)]
pub(crate) struct StateAuthority {
    pub root: PathBuf,
    pub app: zup_core::AppId,
    pub scope: SelectedScope,
}

impl StateAuthority {
    pub fn payload_directory(
        &self,
        path: &Path,
    ) -> Result<crate::filesystem::authority::DirectoryAncestry, FilesystemError> {
        use crate::filesystem::authority::{
            DirectoryAccess, DirectoryAncestry, FileAuthority, FileParent,
        };
        let authority = || -> Result<FileAuthority, FilesystemError> {
            Ok(match self.scope {
                SelectedScope::User => FileAuthority::User(
                    crate::transport::UserSid::current()
                        .map_err(|error| io_error(path, std::io::Error::other(error)))?,
                ),
                SelectedScope::Machine => FileAuthority::Machine,
            })
        };
        let mut candidate = path;
        let parent = loop {
            match DirectoryAncestry::open(candidate) {
                Ok(parent) => break parent,
                Err(FilesystemError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    candidate = candidate.parent().ok_or_else(|| {
                        io_error(
                            path,
                            std::io::Error::other("application resource has no existing parent"),
                        )
                    })?;
                }
                Err(error) => return Err(error),
            }
        };
        let base = match self.scope {
            SelectedScope::User => host_dirs::user_data()?.join(zup_transaction::STATE_FOLDER),
            SelectedScope::Machine => machine_base()?,
        };
        let selected = match self.scope {
            SelectedScope::User => private_fs::PrivateDir::open(&self.root)
                .map_err(|error| match error {
                    private_fs::Error::Io(source) => io_error(&self.root, source),
                    error => io_error(&self.root, std::io::Error::other(error)),
                })
                .and_then(|directory| crate::filesystem::private_directory_identity(&directory)),
            SelectedScope::Machine => crate::filesystem::MachineDir::root_identity(&self.root),
        };
        let namespace = match self.scope {
            SelectedScope::User => FileParent::open(&base, authority()?, DirectoryAccess::Read)
                .and_then(|directory| directory.object_identity()),
            SelectedScope::Machine => crate::filesystem::MachineDir::root_identity(&base),
        };
        for excluded in [selected, namespace] {
            match excluded {
                Ok(excluded) => parent.reject_ancestor(excluded)?,
                Err(FilesystemError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(parent)
    }
    pub fn source(
        &self,
        transaction: zup_core::TransactionId,
    ) -> Result<zup_transaction::TransactionRecord, String> {
        use zup_transaction::TransactionStore;
        let store = match self.scope {
            SelectedScope::User => {
                zup_transaction::FilesystemTransactionStore::private_at(&self.root)
            }
            SelectedScope::Machine => self
                .machine()
                .map_err(|error| error.to_string())?
                .transaction_store(),
        };
        let record = store
            .load(&transaction)
            .map_err(|error| error.to_string())?;
        if record.app_id != self.app || record.scope != self.scope {
            return Err(
                "metadata source differs from the authenticated application authority".into(),
            );
        }
        Ok(record)
    }

    pub fn machine(&self) -> Result<crate::machine_authority::MachineRoot, FilesystemError> {
        if self.scope != SelectedScope::Machine {
            return Err(FilesystemError::Io {
                path: self.root.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "user state has no machine authority",
                ),
            });
        }
        crate::machine_authority::MachineRoot::from_existing(&self.root, &self.app)
    }
}

fn ensure_user_state_root(path: &Path) -> Result<PathBuf, FilesystemError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    zup_storage::ensure_private_dir(path)
        .map_err(|error| io_error(path, std::io::Error::other(error)))?;
    canonical(path)
}

pub(crate) fn machine_base() -> Result<PathBuf, FilesystemError> {
    canonical(&host_dirs::shared_data()?).map(|base| base.join(zup_transaction::STATE_FOLDER))
}

pub fn default_state_root(
    app: &zup_core::AppId,
    scope: SelectedScope,
) -> Result<PathBuf, FilesystemError> {
    let name = app.state_key().to_hex();
    match scope {
        SelectedScope::User => ensure_user_state_root(
            &host_dirs::user_data()?
                .join(zup_transaction::STATE_FOLDER)
                .join("apps")
                .join(name),
        ),
        SelectedScope::Machine => Ok(machine_base()?.join("apps").join(name)),
    }
}

pub fn resolve_state_root(
    app: &zup_core::AppId,
    explicit: Option<PathBuf>,
    scope: SelectedScope,
) -> Result<PathBuf, FilesystemError> {
    match explicit {
        Some(path) if scope == SelectedScope::Machine => {
            let authoritative = default_state_root(app, SelectedScope::Machine)?;
            if path != authoritative {
                return Err(FilesystemError::Io {
                    path,
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "machine state is the elevated worker's root, never a caller path",
                    ),
                });
            }
            Ok(authoritative)
        }
        Some(path) => ensure_user_state_root(&path),
        None => default_state_root(app, scope),
    }
}

fn canonical(path: &Path) -> Result<PathBuf, FilesystemError> {
    path.canonicalize().map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> FilesystemError {
    FilesystemError::Io {
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

    #[test]
    fn explicit_user_state_uses_private_filesystem_authority() {
        let parent = tempfile::tempdir().expect("temporary parent");
        let path = resolve_state_root(
            &zup_core::AppId::new("com.example.root").unwrap(),
            Some(parent.path().join("state")),
            SelectedScope::User,
        )
        .expect("private root");
        private_fs::PrivateDir::open(&path)
            .expect("the explicit root satisfies the same private policy as the default root");
    }

    #[test]
    fn application_resources_remain_outside_native_state_ancestry() {
        let parent = tempfile::tempdir_in(user_data().unwrap()).unwrap();
        let path = ensure_user_state_root(&parent.path().join("state")).unwrap();
        let state = StateAuthority {
            root: path.clone(),
            app: zup_core::AppId::new("com.example.root").unwrap(),
            scope: SelectedScope::User,
        };
        assert!(state.payload_directory(&path.join("new-data")).is_err());
        state
            .payload_directory(&parent.path().join("payload"))
            .unwrap();
    }

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
        let app = zup_core::AppId::new("com.example.root").unwrap();
        let user = default_state_root(&app, SelectedScope::User).expect("a user state root");
        assert_eq!(
            user,
            user_data()
                .expect("a user data directory")
                .canonicalize()
                .expect("the user data root exists")
                .join("zup")
                .join("apps")
                .join(app.state_key().to_hex()),
            "a user install's state lives in the user's own data directory"
        );
        assert!(
            user.is_dir(),
            "a user state root is created: {}",
            user.display()
        );

        let expected = shared_data()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("zup")
            .join("apps")
            .join(app.state_key().to_hex());
        let existed = expected.try_exists().unwrap();
        let machine =
            default_state_root(&app, SelectedScope::Machine).expect("a machine state root");
        assert_eq!(machine, expected);
        assert_eq!(
            machine.try_exists().unwrap(),
            existed,
            "resolving the machine root preserves its presence"
        );
    }
}
