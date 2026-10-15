//! Machine journals, locks, and ledgers on the handle-relative machine filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_transaction::{
    FilesystemTransactionStore, InstallationLock, JournalVolume, StoreError, TransactionId,
};

use crate::machine_fs::{MachineDir, PolicyKind};
use crate::machine_state::{MachineStateError, default_state_root};

#[derive(Clone)]
pub struct MachineRoot {
    path: PathBuf,
    root: Arc<MachineDir>,
}

impl MachineRoot {
    pub fn acquire() -> Result<Self, MachineStateError> {
        let path = default_state_root(zup_core::SelectedScope::Machine)?;
        Self::establish_at(&path)
    }

    pub fn establish_at(path: &Path) -> Result<Self, MachineStateError> {
        let root = MachineDir::open_root(path)?;
        let state = Self {
            path: path.to_path_buf(),
            root: Arc::new(root),
        };
        state.child("transactions", PolicyKind::Administrators)?;
        state.child("locks", PolicyKind::Administrators)?;
        state.child("installations", PolicyKind::Ledger)?;
        Ok(state)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn transaction_store(&self) -> FilesystemTransactionStore {
        FilesystemTransactionStore::from_volume(Arc::new(MachineJournals {
            root: Arc::clone(&self.root),
        }))
    }

    pub fn try_lock(&self, key: &str) -> Result<Option<InstallationLock>, MachineStateError> {
        let locks = self.child("locks", PolicyKind::Administrators)?;
        let file = locks.open_lock(&format!("{key}.lock"))?;
        InstallationLock::try_lock(file, key.to_owned()).map_err(|error| MachineStateError::Io {
            path: self.path.join("locks"),
            source: std::io::Error::other(error),
        })
    }

    pub fn read_installation(&self, name: &str) -> Result<Option<Vec<u8>>, MachineStateError> {
        self.child("installations", PolicyKind::Ledger)?
            .read(name, PolicyKind::Ledger)
    }

    pub fn publish_installation(&self, name: &str, bytes: &[u8]) -> Result<(), MachineStateError> {
        self.child("installations", PolicyKind::Ledger)?
            .publish(name, bytes, false, PolicyKind::Ledger)
            .map_err(|error| MachineStateError::Io {
                path: self.path.join("installations").join(name),
                source: std::io::Error::other(error),
            })
    }

    pub fn remove_installation(&self, name: &str) -> Result<(), MachineStateError> {
        let directory = self.child("installations", PolicyKind::Ledger)?;
        match directory.read(name, PolicyKind::Ledger)? {
            Some(_) => directory.remove(name),
            None => Ok(()),
        }
    }

    pub fn list_journals(&self) -> Result<Vec<TransactionId>, MachineStateError> {
        let names = self
            .child("transactions", PolicyKind::Administrators)?
            .list()?;
        let mut ids = Vec::new();
        for name in names {
            if let Ok(id) = name.parse::<TransactionId>() {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub fn remove_journal(&self, id: &TransactionId) -> Result<(), MachineStateError> {
        self.child("transactions", PolicyKind::Administrators)?
            .remove_directory(&id.to_string())
    }

    fn child(&self, name: &str, kind: PolicyKind) -> Result<MachineDir, MachineStateError> {
        self.root.ensure_dir(name, kind)
    }
}

struct MachineJournals {
    root: Arc<MachineDir>,
}

impl JournalVolume for MachineJournals {
    fn read_journal(&self, id: &TransactionId) -> Result<Option<Vec<u8>>, StoreError> {
        let transactions = self.transactions()?;
        let Some(directory) = open_optional(&transactions, &id.to_string())? else {
            return Ok(None);
        };
        directory
            .read("transaction.json", PolicyKind::Administrators)
            .map_err(|error| store_io(id, error))
    }

    fn publish_journal(
        &self,
        id: &TransactionId,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<(), StoreError> {
        let transactions = self.transactions()?;
        let directory = transactions
            .ensure_dir(&id.to_string(), PolicyKind::Administrators)
            .map_err(|error| store_io(id, error))?;
        directory
            .publish(
                "transaction.json",
                bytes,
                create_new,
                PolicyKind::Administrators,
            )
            .map_err(|source| StoreError::Publication {
                path: std::path::PathBuf::from(id.to_string()).join("transaction.json"),
                source,
            })
    }

    fn lock_journal(&self, id: &TransactionId) -> Result<std::fs::File, StoreError> {
        let transactions = self.transactions()?;
        let directory = transactions
            .ensure_dir(&id.to_string(), PolicyKind::Administrators)
            .map_err(|error| store_io(id, error))?;
        directory
            .open_lock("transaction.lock")
            .map_err(|error| store_io(id, error))
            .and_then(|file| {
                file.lock().map_err(|source| StoreError::Io {
                    path: std::path::PathBuf::from(id.to_string()).join("transaction.lock"),
                    source,
                })?;
                Ok(file)
            })
    }

    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError> {
        let transactions = self.transactions()?;
        let names = transactions.list().map_err(|error| StoreError::Io {
            path: std::path::PathBuf::from("transactions"),
            source: std::io::Error::other(error),
        })?;
        let mut ids = Vec::new();
        for name in names {
            if let Ok(id) = name.parse::<TransactionId>() {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

impl MachineJournals {
    fn transactions(&self) -> Result<MachineDir, StoreError> {
        self.root
            .ensure_dir("transactions", PolicyKind::Administrators)
            .map_err(|error| StoreError::Io {
                path: std::path::PathBuf::from("transactions"),
                source: std::io::Error::other(error),
            })
    }
}

fn open_optional(parent: &MachineDir, name: &str) -> Result<Option<MachineDir>, StoreError> {
    match parent.open_dir(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(MachineStateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(StoreError::Io {
            path: PathBuf::from(name),
            source: std::io::Error::other(error),
        }),
    }
}

fn store_io(id: &TransactionId, error: MachineStateError) -> StoreError {
    StoreError::Io {
        path: PathBuf::from(id.to_string()),
        source: std::io::Error::other(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_state_follows_the_token_privilege_contract() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("zup");
        match MachineRoot::establish_at(&path) {
            Ok(machine) => {
                assert!(machine.path() == path);
                assert!(
                    machine.list_journals().expect("journal list").is_empty(),
                    "a new machine root has no journals"
                );
                let weakened = path.join("transactions");
                weaken_dacl(&weakened);
                let again = MachineRoot::establish_at(&path);
                assert!(
                    again.is_err(),
                    "a directory whose DACL no longer matches the policy is refused"
                );
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("elevated") || message.contains("SeRestorePrivilege"),
                    "refusal must name the missing authority: {message}"
                );
                assert!(
                    !path.exists(),
                    "a refused machine root must not leave a directory behind"
                );
            }
        }
    }

    #[test]
    fn a_second_process_sees_the_machine_lock_as_busy() {
        if std::env::var("ZUP_MACHINE_LOCK_CHILD").is_ok() {
            let path = std::env::var("ZUP_MACHINE_LOCK_PATH").expect("path");
            let root = MachineRoot::establish_at(Path::new(&path)).expect("child root");
            let _held = root
                .try_lock("zup-install-probe-machine")
                .expect("lock")
                .expect("the child holds the lock");
            std::fs::write(
                std::env::var("ZUP_MACHINE_LOCK_READY").expect("ready"),
                b"held",
            )
            .expect("ready");
            let release = std::env::var("ZUP_MACHINE_LOCK_RELEASE").expect("release");
            while !Path::new(&release).exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            return;
        }
        let directory = tempfile::tempdir().expect("temp");
        let path = directory.path().join("zup");
        let root = match MachineRoot::establish_at(&path) {
            Ok(root) => root,
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("elevated") || message.contains("SeRestorePrivilege"),
                    "{message}"
                );
                return;
            }
        };
        let ready = directory.path().join("ready");
        let release = directory.path().join("release");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .env("ZUP_MACHINE_LOCK_CHILD", "1")
            .env("ZUP_MACHINE_LOCK_PATH", &path)
            .env("ZUP_MACHINE_LOCK_READY", &ready)
            .env("ZUP_MACHINE_LOCK_RELEASE", &release)
            .args([
                "--exact",
                "machine_authority::tests::a_second_process_sees_the_machine_lock_as_busy",
            ])
            .spawn()
            .expect("child");
        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(ready.exists(), "the child process did not acquire the lock");
        assert!(
            root.try_lock("zup-install-probe-machine")
                .expect("contender")
                .is_none()
        );
        std::fs::write(&release, b"go").expect("release");
        assert!(child.wait().expect("wait").success());
    }

    fn weaken_dacl(path: &Path) {
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
            .chain(Some(0))
            .collect();
        let status = unsafe {
            windows::Win32::Security::Authorization::SetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Security::Authorization::SE_FILE_OBJECT,
                windows::Win32::Security::OBJECT_SECURITY_INFORMATION(0x4 | 0x8000_0000),
                None,
                None,
                None,
                None,
            )
        };
        assert_eq!(status.0, 0, "the test token can change the machine DACL");
    }
}
