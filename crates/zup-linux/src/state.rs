use std::path::{Path, PathBuf};

use zup_core::{AppId, SelectedScope, TransactionId};
use zup_transaction::{TransactionRecord, TransactionStore};

use crate::error::ExecError;

#[derive(Clone)]
pub(crate) struct StateAuthority {
    pub root: PathBuf,
    pub app: AppId,
    pub scope: SelectedScope,
}

impl StateAuthority {
    pub fn payload_directory(&self, path: &Path) -> Result<crate::fs::OwnedDirectory, ExecError> {
        let mut candidate = path;
        let parent = loop {
            match crate::fs::OwnedDirectory::open(candidate) {
                Ok(parent) => break parent,
                Err(crate::error::PathError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    candidate = candidate.parent().ok_or_else(|| {
                        ExecError::refused("application resource", "no existing parent directory")
                    })?;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let base = match self.scope {
            SelectedScope::User => xdg::BaseDirectories::new()
                .get_state_home()
                .map(|path| path.join("zup")),
            SelectedScope::Machine => Some(crate::locations::MachineRoots::production().state),
        };
        for root in std::iter::once(&self.root).chain(base.as_ref()) {
            match crate::fs::OwnedDirectory::open(root) {
                Ok(forbidden) => parent.reject_ancestor(forbidden.stat_self()?.object())?,
                Err(crate::error::PathError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(parent)
    }
    pub fn source(&self, transaction: TransactionId) -> Result<TransactionRecord, ExecError> {
        let record = zup_transaction::FilesystemTransactionStore::private_at(&self.root)
            .load(&transaction)
            .map_err(|error| ExecError::Executor(error.to_string()))?;
        self.validate(&record)?;
        Ok(record)
    }

    pub fn validate(&self, record: &TransactionRecord) -> Result<(), ExecError> {
        if record.app_id != self.app || record.scope != self.scope {
            return Err(ExecError::refused(
                "installation state",
                "protected transaction belongs to another application or scope",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_placement_refuses_native_state_ancestry_and_redirection() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        zup_storage::ensure_private_dir(&root).unwrap();
        let state = StateAuthority {
            root: root.clone(),
            app: AppId::new("com.example.state-boundary").unwrap(),
            scope: SelectedScope::User,
        };
        assert!(state.payload_directory(&root.join("new-data")).is_err());
        assert!(
            state
                .payload_directory(&temporary.path().join("payload"))
                .is_ok()
        );
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(state.payload_directory(&alias.join("new-data")).is_err());
        assert!(std::fs::read_dir(root).unwrap().next().is_none());
    }
}
