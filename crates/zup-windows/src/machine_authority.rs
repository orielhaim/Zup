//! Machine journals, locks, and ledgers on the handle-relative machine filesystem.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zup_core::TransactionId;
#[cfg(test)]
use zup_plan::{DeploymentEffects, compile_deployment};

use zup_transaction::{FilesystemTransactionStore, JournalVolume, ResourceLock, StoreError};

use crate::filesystem::FilesystemError;
use crate::filesystem::{MachineDir, PolicyKind};
use crate::machine_state::default_state_root;

#[derive(Clone)]
pub struct MachineRoot {
    path: PathBuf,
    root: Arc<MachineDir>,
}

impl MachineRoot {
    pub fn acquire(app: &zup_core::AppId) -> Result<Self, FilesystemError> {
        let base = MachineDir::open_root(&crate::machine_state::machine_base()?)?;
        let apps = base.ensure_dir("apps", PolicyKind::Root)?;
        let root = apps.ensure_dir(&app.state_key().to_hex(), PolicyKind::Root)?;
        Self::establish(
            default_state_root(app, zup_core::SelectedScope::Machine)?,
            root,
        )
    }

    pub fn from_existing(path: &Path, app: &zup_core::AppId) -> Result<Self, FilesystemError> {
        let expected = default_state_root(app, zup_core::SelectedScope::Machine)?;
        if path != expected {
            return Err(FilesystemError::Io {
                path: path.to_owned(),
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "machine state differs from the authenticated application root",
                ),
            });
        }
        let base = MachineDir::open_root(&crate::machine_state::machine_base()?)?;
        let root = base
            .open_dir("apps", PolicyKind::Root)?
            .open_dir(&app.state_key().to_hex(), PolicyKind::Root)?;
        Self::establish(expected, root)
    }

    pub fn shared_lock(key: &str) -> Result<Option<ResourceLock>, FilesystemError> {
        let path = crate::machine_state::machine_base()?;
        let base = MachineDir::open_root(&path)?;
        let locks = base.ensure_dir("locks", PolicyKind::Administrators)?;
        let file = locks.open_lock(&format!("{key}.lock"))?;
        ResourceLock::try_lock(file, key.to_owned()).map_err(|error| FilesystemError::Io {
            path: path.join("locks"),
            source: io::Error::other(error),
        })
    }

    #[cfg(test)]
    fn establish_at(path: &Path) -> Result<Self, FilesystemError> {
        let root = MachineDir::open_root(path)?;
        Self::establish(path.to_owned(), root)
    }

    fn establish(path: PathBuf, root: MachineDir) -> Result<Self, FilesystemError> {
        let state = Self {
            path,
            root: Arc::new(root),
        };
        state
            .root
            .ensure_dir("transactions", PolicyKind::Administrators)?;
        state.root.ensure_dir("locks", PolicyKind::Administrators)?;
        state.root.ensure_dir("installations", PolicyKind::Ledger)?;
        Ok(state)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn state_directory(
        &self,
        namespace: zup_transaction::StateNamespace,
        create: bool,
    ) -> Result<MachineDir, FilesystemError> {
        use zup_transaction::StateNamespace;
        let name = match namespace {
            StateNamespace::Transactions => "transactions",
            StateNamespace::History => "history",
            StateNamespace::Materializations(_) => "materializations",
            StateNamespace::Data => "data",
            StateNamespace::Collections => "collections",
        };
        let parent = if create {
            self.root.ensure_dir(name, PolicyKind::Administrators)?
        } else {
            self.root.open_dir(name, PolicyKind::Administrators)?
        };
        if let StateNamespace::Materializations(installation) = namespace {
            if create {
                parent.ensure_dir(&installation.to_string(), PolicyKind::Administrators)
            } else {
                parent.open_dir(&installation.to_string(), PolicyKind::Administrators)
            }
        } else {
            Ok(parent)
        }
    }

    pub(crate) fn repository_store(
        &self,
        context: &zup_update::TrustContext,
    ) -> std::io::Result<Arc<dyn tough::MetadataStore>> {
        use std::io;
        if context.state_root != self.path {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "repository state is outside the machine authority",
            ));
        }
        let anchor = context.trust.trust_anchor().map_err(io::Error::other)?;
        let directory = self
            .root
            .ensure_dir("repositories", PolicyKind::Administrators)
            .and_then(|parent| {
                parent.ensure_dir(context.trust.app_id.as_str(), PolicyKind::Administrators)
            })
            .and_then(|parent| parent.ensure_dir(&anchor.to_hex(), PolicyKind::Administrators))
            .map_err(io::Error::other)?;
        let lease = directory
            .open_lock("repository.lock")
            .map_err(io::Error::other)?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another update is using this repository trust state",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        zup_update::repository_state::open(
            context,
            MachineRepositoryFile {
                directory,
                _lease: lease,
            },
        )
    }

    pub fn transaction_store(&self) -> FilesystemTransactionStore {
        FilesystemTransactionStore::from_volume(Arc::new(MachineJournals {
            root: Arc::clone(&self.root),
        }))
    }

    pub(crate) fn transaction_directory(
        &self,
        id: TransactionId,
    ) -> Result<MachineDir, FilesystemError> {
        self.root
            .open_dir("transactions", PolicyKind::Administrators)?
            .open_dir(&id.to_string(), PolicyKind::Administrators)
    }

    pub fn try_lock(&self, key: &str) -> Result<Option<ResourceLock>, FilesystemError> {
        let locks = self.child("locks", PolicyKind::Administrators)?;
        let file = locks.open_lock(&format!("{key}.lock"))?;
        ResourceLock::try_lock(file, key.to_owned()).map_err(|error| FilesystemError::Io {
            path: self.path.join("locks"),
            source: std::io::Error::other(error),
        })
    }

    pub fn read_installation(&self, name: &str) -> Result<Option<Vec<u8>>, FilesystemError> {
        self.child("installations", PolicyKind::Ledger)?
            .read(name, PolicyKind::Ledger)
    }

    pub fn publish_installation(&self, name: &str, bytes: &[u8]) -> Result<(), FilesystemError> {
        self.child("installations", PolicyKind::Ledger)?
            .publish(name, bytes, false, PolicyKind::Ledger)
            .map_err(|error| FilesystemError::Io {
                path: self.path.join("installations").join(name),
                source: std::io::Error::other(error),
            })
    }

    pub fn remove_installation(&self, name: &str) -> Result<(), FilesystemError> {
        let directory = self.child("installations", PolicyKind::Ledger)?;
        directory.remove(name)
    }

    #[cfg(test)]
    pub fn remove_journal(&self, id: &TransactionId) -> Result<(), FilesystemError> {
        self.child("transactions", PolicyKind::Administrators)?
            .remove_directory(&id.to_string())
    }

    fn child(&self, name: &str, kind: PolicyKind) -> Result<MachineDir, FilesystemError> {
        self.root.open_dir(name, kind)
    }
}

struct MachineJournals {
    root: Arc<MachineDir>,
}

impl JournalVolume for MachineJournals {
    fn read_reservation(
        &self,
        id: zup_core::InstallationId,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let directory = self
            .root
            .ensure_dir("identities", PolicyKind::Administrators)
            .map_err(|error| StoreError::Io {
                path: "identities".into(),
                source: std::io::Error::other(error),
            })?;
        let bytes = directory
            .read(&format!("{id}.json"), PolicyKind::Administrators)
            .map_err(|error| StoreError::Io {
                path: "identities".into(),
                source: std::io::Error::other(error),
            })?;
        directory.sync().map_err(|error| StoreError::Io {
            path: "identities".into(),
            source: std::io::Error::other(error),
        })?;
        Ok(bytes)
    }

    fn create_reservation(
        &self,
        id: zup_core::InstallationId,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        let directory = self
            .root
            .ensure_dir("identities", PolicyKind::Administrators)
            .map_err(|error| StoreError::Io {
                path: "identities".into(),
                source: std::io::Error::other(error),
            })?;
        directory
            .publish(
                &format!("{id}.json"),
                bytes,
                true,
                PolicyKind::Administrators,
            )
            .map_err(|source| StoreError::Publication {
                path: std::path::PathBuf::from("identities").join(format!("{id}.json")),
                source,
            })
    }
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
            let id = name
                .parse::<TransactionId>()
                .map_err(|_| StoreError::InvalidDirectory { name })?;
            ids.push(id);
        }
        Ok(ids)
    }
}

impl MachineJournals {
    fn transactions(&self) -> Result<MachineDir, StoreError> {
        self.root
            .open_dir("transactions", PolicyKind::Administrators)
            .map_err(|error| StoreError::Io {
                path: std::path::PathBuf::from("transactions"),
                source: std::io::Error::other(error),
            })
    }
}

fn open_optional(parent: &MachineDir, name: &str) -> Result<Option<MachineDir>, StoreError> {
    match parent.open_dir(name, PolicyKind::Administrators) {
        Ok(directory) => Ok(Some(directory)),
        Err(FilesystemError::Io { source, .. })
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

fn store_io(id: &TransactionId, error: FilesystemError) -> StoreError {
    StoreError::Io {
        path: PathBuf::from(id.to_string()),
        source: std::io::Error::other(error),
    }
}

struct MachineRepositoryFile {
    directory: MachineDir,
    _lease: std::fs::File,
}

impl zup_update::repository_state::RepositoryStateFile for MachineRepositoryFile {
    fn read(&self) -> std::io::Result<Option<Vec<u8>>> {
        use zup_update::repository_state::{MAX_STATE_BYTES, STATE_FILE};
        let bytes = self
            .directory
            .read_bounded(STATE_FILE, PolicyKind::Administrators, MAX_STATE_BYTES)
            .map_err(std::io::Error::other)?;
        self.directory.sync().map_err(std::io::Error::other)?;
        Ok(bytes)
    }

    fn publish(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.directory
            .publish(
                zup_update::repository_state::STATE_FILE,
                bytes,
                false,
                PolicyKind::Administrators,
            )
            .map_err(std::io::Error::other)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn machine_state_follows_the_token_privilege_contract() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("zup");
        if !crate::transport::is_process_elevated().expect("token elevation") {
            assert!(MachineRoot::establish_at(&path).is_err());
            assert!(!path.exists());
            return;
        }
        let machine = MachineRoot::establish_at(&path).expect("elevated root");
        assert_eq!(machine.path(), path);
        assert!(
            machine
                .transaction_store()
                .list_ids()
                .expect("journal list")
                .is_empty()
        );
        weaken_dacl(&path.join("transactions"));
        assert!(MachineRoot::establish_at(&path).is_err());
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn machine_repository_state_recovers_a_committed_batch_after_process_exit() {
        const CHILD: &str = "ZUP_MACHINE_REPOSITORY_CHILD";
        let inherited = std::env::var_os(CHILD);
        let temporary = inherited.is_none().then(|| tempfile::tempdir().unwrap());
        let path = inherited
            .map(PathBuf::from)
            .unwrap_or_else(|| temporary.as_ref().unwrap().path().join("state"));
        let machine = MachineRoot::establish_at(&path).unwrap();
        let context = zup_update::TrustContext::from_update_config(
            &zup_core::UpdateConfig {
                repository: "https://example.test/releases".into(),
                channel: "stable".into(),
                trusted_root: b"machine repository fixture".to_vec(),
            },
            zup_core::AppId::new("com.example.machine-repository").unwrap(),
            zup_core::InstallScope::Machine,
            &path,
        );
        if temporary.is_none() {
            let store = machine.repository_store(&context).unwrap();
            store
                .apply(&[
                    tough::MetadataChange::Write {
                        name: "root.json".into(),
                        bytes: b"root two".to_vec(),
                    },
                    tough::MetadataChange::Remove {
                        name: "timestamp.json".into(),
                    },
                    tough::MetadataChange::Remove {
                        name: "snapshot.json".into(),
                    },
                ])
                .unwrap();
            std::process::abort();
        }
        let store = machine.repository_store(&context).unwrap();
        store
            .apply(&[
                tough::MetadataChange::Write {
                    name: "root.json".into(),
                    bytes: b"root one".to_vec(),
                },
                tough::MetadataChange::Write {
                    name: "timestamp.json".into(),
                    bytes: b"timestamp five".to_vec(),
                },
                tough::MetadataChange::Write {
                    name: "snapshot.json".into(),
                    bytes: b"snapshot five".to_vec(),
                },
            ])
            .unwrap();
        assert!(machine.repository_store(&context).is_err());
        drop(store);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "machine_authority::tests::machine_repository_state_recovers_a_committed_batch_after_process_exit", "--ignored"])
            .env(CHILD, &path).output().unwrap();
        assert!(!child.status.success());
        let recovered = machine.repository_store(&context).unwrap();
        assert_eq!(recovered.bytes("root.json").unwrap().unwrap(), b"root two");
        assert!(recovered.bytes("timestamp.json").unwrap().is_none());
        assert!(recovered.bytes("snapshot.json").unwrap().is_none());
    }

    #[test]
    #[ignore = "requires elevated administrator"]
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
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !Path::new(&release).exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent never released child"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            return;
        }
        let directory = tempfile::tempdir().expect("temp");
        let path = directory.path().join("zup");
        let root = MachineRoot::establish_at(&path).expect("elevated root");
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
                "--ignored",
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

    #[test]
    #[ignore = "requires elevated administrator"]
    fn machine_journals_use_shared_revision_cas_and_reopen() {
        use zup_transaction::{TransactionRecord, TransactionStore};
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("state");
        let machine = MachineRoot::establish_at(&path).expect("machine");
        let store = machine.transaction_store();
        let orphan = TransactionId::new_v7();
        let volume = MachineJournals {
            root: Arc::clone(&machine.root),
        };
        drop(
            volume
                .lock_journal(&orphan)
                .expect("initial unpublished journal"),
        );
        assert_eq!(store.list_ids().expect("journal directories"), vec![orphan]);
        assert!(matches!(
            store.load(&orphan),
            Err(StoreError::Corrupt(zup_transaction::CorruptReason::Missing))
        ));
        machine
            .remove_journal(&orphan)
            .expect("validated orphan cleanup");
        let target = zup_core::Target::host().unwrap();
        let mut effects = DeploymentEffects::new(
            zup_core::InstallationId::new_v7(),
            crate::requirements_fixture::requirements(
                target,
                zup_core::SelectedScope::Machine,
                "probe",
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(zup_core::TargetPath::new(target, crate::plain_path_text(&path)).unwrap());
        let plan = compile_deployment(&effects).expect("barrier plan");
        let record = TransactionRecord::new(
            TransactionId::new_v7(),
            zup_core::AppId::new("probe").expect("app"),
            zup_core::SelectedScope::Machine,
            "1.0.0".parse().expect("version"),
            plan,
        );
        store.create(&record).expect("create");
        assert!(matches!(
            store.create(&record),
            Err(StoreError::AlreadyExists { .. })
        ));
        let mut next = record.clone();
        next.touch();
        store.compare_and_swap(0, &next).expect("CAS");
        assert!(matches!(
            store.compare_and_swap(0, &next),
            Err(StoreError::RevisionConflict { .. })
        ));
        let reopened = MachineRoot::establish_at(&path).expect("reopen");
        assert_eq!(
            reopened
                .transaction_store()
                .load(&record.transaction_id)
                .expect("recover journal"),
            next
        );
        assert_eq!(
            reopened.transaction_store().list_ids().expect("list"),
            [record.transaction_id]
        );
        machine
            .publish_installation("probe.json", b"ledger")
            .expect("ledger");
        assert_eq!(
            reopened
                .read_installation("probe.json")
                .expect("read ledger"),
            Some(b"ledger".to_vec())
        );
        machine
            .remove_installation("probe.json")
            .expect("remove ledger");
        assert!(
            reopened
                .read_installation("probe.json")
                .expect("absent ledger")
                .is_none()
        );
        machine
            .remove_journal(&record.transaction_id)
            .expect("remove journal");
        assert!(
            reopened
                .transaction_store()
                .list_ids()
                .expect("list")
                .is_empty()
        );
    }

    pub(crate) fn weaken_dacl(path: &Path) {
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
