use std::path::{Path, PathBuf};
use std::sync::Arc;
use zup_storage::PublishError;

use crate::JournalDirectory;
#[path = "private_journals.rs"]
mod private_journals;
use private_journals::PrivateJournalDirectory;

use crate::record::{CorruptReason, StoreError, TransactionRecord};
use zup_core::TransactionId;

/// Directory capability that already carries its security policy.
///
/// The transaction coordinator never opens a raw path and decides the policy
/// itself. Callers at the trust boundary choose a private volume or a
/// platform machine volume.
pub trait JournalVolume: Send + Sync {
    fn read_reservation(&self, id: zup_core::InstallationId)
    -> Result<Option<Vec<u8>>, StoreError>;
    fn create_reservation(
        &self,
        id: zup_core::InstallationId,
        bytes: &[u8],
    ) -> Result<(), StoreError>;
    fn open_journal(
        &self,
        id: &TransactionId,
    ) -> Result<Option<Box<dyn crate::LockedJournal>>, StoreError>;
    fn create_journal(&self, id: &TransactionId, bytes: &[u8]) -> Result<(), StoreError>;
    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError>;
}

const MAX_UPDATE_ATTEMPTS: u32 = 8;

pub trait TransactionStore {
    fn reserve_installation(
        &self,
        record: &TransactionRecord,
    ) -> Result<crate::InstallationReservation, StoreError>;
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError>;
    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError>;
    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError>;

    fn update(
        &self,
        id: &TransactionId,
        change: &mut dyn FnMut(&mut TransactionRecord) -> Result<(), StoreError>,
    ) -> Result<TransactionRecord, StoreError> {
        for _ in 0..MAX_UPDATE_ATTEMPTS {
            let mut next = self.load(id)?;
            let expected = next.revision;
            change(&mut next)?;
            next.revision = expected;
            next.touch();
            match self.compare_and_swap(expected, &next) {
                Ok(()) => return Ok(next),
                Err(StoreError::RevisionConflict { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(StoreError::UpdateExhausted {
            id: id.to_string(),
            attempts: MAX_UPDATE_ATTEMPTS,
        })
    }
}

#[derive(Clone)]
pub struct FilesystemTransactionStore {
    volume: Arc<dyn JournalVolume>,
}

impl FilesystemTransactionStore {
    /// Owner-private journals. Linux root-owned machine state uses this when
    /// the process owner matches the private-fs contract. Windows machine state
    /// does not.
    pub fn private_at(root: impl Into<PathBuf>) -> Self {
        Self {
            volume: Arc::new(PrivateJournals::new(root.into())),
        }
    }

    pub fn from_volume(volume: Arc<dyn JournalVolume>) -> Self {
        Self { volume }
    }

    pub fn list_ids(&self) -> Result<Vec<TransactionId>, StoreError> {
        let ids = self.volume.list_journals()?;
        for id in &ids {
            self.load(id)?;
        }
        Ok(ids)
    }
}

struct PrivateJournals {
    root: PathBuf,
}

impl PrivateJournals {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn namespace(
        &self,
        name: &str,
        create: bool,
    ) -> Result<Option<PrivateJournalDirectory>, StoreError> {
        let root = match private_fs::PrivateDir::open(&self.root) {
            Ok(root) => root,
            Err(private_fs::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                zup_storage::ensure_private_dir(&self.root)
                    .map_err(|error| store_fs(self.root.clone(), error))?
            }
            Err(error) => return Err(store_fs(self.root.clone(), error)),
        };
        let root = PrivateJournalDirectory(root);
        if !create {
            return root.open(name);
        }
        let directory = PrivateJournalDirectory(
            root.0
                .ensure_dir(name)
                .map_err(|error| store_fs(self.root.join(name), error))?,
        );
        if !root.same_mount(&directory)? {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        root.sync()?;
        Ok(Some(directory))
    }
}

impl JournalVolume for PrivateJournals {
    fn read_reservation(
        &self,
        id: zup_core::InstallationId,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(directory) = self.namespace("identities", false)? else {
            return Ok(None);
        };
        let bytes = directory.read(&format!("{id}.json"))?;
        if bytes.is_some() {
            directory.sync()?;
        }
        Ok(bytes)
    }
    fn create_reservation(
        &self,
        id: zup_core::InstallationId,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        self.namespace("identities", true)?
            .ok_or(CorruptReason::Missing)?
            .publish(&format!("{id}.json"), bytes, true)
    }
    fn open_journal(
        &self,
        id: &TransactionId,
    ) -> Result<Option<Box<dyn crate::LockedJournal>>, StoreError> {
        let Some(namespace) = self.namespace("transactions", false)? else {
            return Ok(None);
        };
        crate::JournalLease::open(namespace, *id)
            .map(|journal| journal.map(|lease| Box::new(lease) as Box<dyn crate::LockedJournal>))
    }
    fn create_journal(&self, id: &TransactionId, bytes: &[u8]) -> Result<(), StoreError> {
        let creation = self
            .namespace("journal-creation", true)?
            .ok_or(CorruptReason::Missing)?;
        let transactions = self
            .namespace("transactions", true)?
            .ok_or(CorruptReason::Missing)?;
        crate::create_journal(&creation, &transactions, *id, bytes)
    }
    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError> {
        let Some(directory) = self.namespace("transactions", false)? else {
            return Ok(Vec::new());
        };
        directory
            .names()?
            .into_iter()
            .map(|name| {
                name.parse::<TransactionId>()
                    .map_err(|_| StoreError::InvalidDirectory { name })
            })
            .collect()
    }
}

fn store_fs(path: PathBuf, error: private_fs::Error) -> StoreError {
    match error {
        private_fs::Error::Io(source) => StoreError::Io { path, source },
        source => StoreError::Publication {
            path,
            source: PublishError::NotPublished {
                kind: None,
                detail: source.to_string(),
            },
        },
    }
}

fn decode_record(bytes: &[u8]) -> Result<TransactionRecord, StoreError> {
    let record: TransactionRecord = serde_json::from_slice(bytes)
        .map_err(|error| CorruptReason::InvalidJson(error.to_string()))?;
    record.validate()?;
    Ok(record)
}

impl TransactionStore for FilesystemTransactionStore {
    fn reserve_installation(
        &self,
        record: &TransactionRecord,
    ) -> Result<crate::InstallationReservation, StoreError> {
        record.validate()?;
        let proposed =
            crate::InstallationReservation::for_transaction(record).map_err(|reason| {
                StoreError::Rejected {
                    id: record.transaction_id.to_string(),
                    reason,
                }
            })?;
        let durable = self.load(&record.transaction_id)?;
        if durable.plan_hash != record.plan_hash
            || durable.app_id != record.app_id
            || durable.scope != record.scope
        {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        if !matches!(
            durable
                .nodes
                .get(&zup_plan::OperationId::new(zup_plan::OperationId::RESERVE)),
            Some(crate::NodeState::Running | crate::NodeState::Applied { .. })
        ) {
            return Err(StoreError::Rejected {
                id: record.transaction_id.to_string(),
                reason: "installation reservation has no durable request".into(),
            });
        }
        let id = proposed.installation;
        if self.volume.read_reservation(id)?.is_none() {
            if record.reservation.is_some() || record.plan.observed.is_some() {
                return Err(CorruptReason::Missing.into());
            }
            let bytes = serde_json::to_vec(&proposed).map_err(StoreError::Serialize)?;
            match self.volume.create_reservation(id, &bytes) {
                Ok(()) => {}
                Err(StoreError::Publication { source, .. })
                    if zup_storage::absent_collision(&source) => {}
                Err(error) => return Err(error),
            }
        }
        let bytes = self
            .volume
            .read_reservation(id)?
            .ok_or(CorruptReason::Missing)?;
        let reserved: crate::InstallationReservation = serde_json::from_slice(&bytes)
            .map_err(|error| CorruptReason::InvalidJson(error.to_string()))?;
        if !reserved.matches(record) {
            return Err(StoreError::InstallationConflict { installation: id });
        }
        let origin = self.load(&reserved.origin)?;
        if !reserved.matches(&origin) {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        if origin.reservation.as_ref() != Some(&reserved)
            && !(origin.reservation.is_none()
                && matches!(
                    origin
                        .nodes
                        .get(&zup_plan::OperationId::new(zup_plan::OperationId::RESERVE)),
                    Some(crate::NodeState::Running)
                ))
        {
            return Err(CorruptReason::InvalidReceipt.into());
        }
        Ok(reserved)
    }
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        record.validate()?;
        let id = record.transaction_id;
        let bytes = serde_json::to_vec_pretty(record).map_err(StoreError::Serialize)?;
        match self.volume.create_journal(&id, &bytes) {
            Ok(()) => Ok(()),
            Err(StoreError::Publication { source, .. })
                if zup_storage::absent_collision(&source) =>
            {
                Err(StoreError::AlreadyExists { id: id.to_string() })
            }
            Err(error) => Err(error),
        }
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        match self.volume.open_journal(id)? {
            Some(journal) => {
                let bytes = journal.read()?;
                let record = decode_record(&bytes)?;
                if record.transaction_id != *id {
                    return Err(CorruptReason::IdentityMismatch.into());
                }
                Ok(record)
            }
            None => Err(StoreError::Corrupt(CorruptReason::Missing)),
        }
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError> {
        updated.validate()?;
        if updated.revision != expected_revision.saturating_add(1) {
            return Err(CorruptReason::RevisionMismatch {
                expected: expected_revision,
                found: updated.revision,
            }
            .into());
        }
        let id = updated.transaction_id;
        let journal = self
            .volume
            .open_journal(&id)?
            .ok_or(CorruptReason::Missing)?;
        let current = decode_record(&journal.read()?)?;
        if current.transaction_id != id {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        if current.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
            });
        }
        let bytes = serde_json::to_vec_pretty(updated).map_err(StoreError::Serialize)?;
        crate::fault_injection::boundary(
            crate::fault_injection::Boundary::JournalCasBefore,
            &id.to_string(),
        );
        journal.publish(&bytes)?;
        crate::fault_injection::boundary(
            crate::fault_injection::Boundary::JournalCasPublished,
            &id.to_string(),
        );
        Ok(())
    }
}

use semver::Version;
use sha2::{Digest, Sha256};
use zup_core::{AppId, SelectedScope, Sha256Digest, Target};

pub const CONTENT_STORE_DIRECTORY: &str = ".zup-content";

const IDENTITY_DOMAIN: &[u8] = b"zup/content-store/identity/v1\0";
const APP_DOMAIN: &[u8] = b"zup/content-store/app/v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentStoreIdentity {
    app_id: AppId,
    version: Version,
    scope: SelectedScope,
    target: Target,
    artifact: Sha256Digest,
}

impl ContentStoreIdentity {
    pub fn new(
        app_id: AppId,
        version: Version,
        scope: SelectedScope,
        target: Target,
        artifact: Sha256Digest,
    ) -> Self {
        Self {
            app_id,
            version,
            scope,
            target,
            artifact,
        }
    }

    pub fn digest(&self) -> Sha256Digest {
        let mut hasher = Sha256::new();
        field(&mut hasher, IDENTITY_DOMAIN);
        field(&mut hasher, self.app_id.as_str().as_bytes());
        field(&mut hasher, self.version.to_string().as_bytes());
        field(&mut hasher, self.scope.to_string().as_bytes());
        field(&mut hasher, self.target.as_str().as_bytes());
        field(&mut hasher, self.artifact.as_bytes());
        Sha256Digest::from_hasher(hasher)
    }

    pub fn path_under(&self, base_root: &Path) -> PathBuf {
        let mut app_hasher = Sha256::new();
        field(&mut app_hasher, APP_DOMAIN);
        field(&mut app_hasher, self.app_id.as_str().as_bytes());
        let app = Sha256Digest::from_hasher(app_hasher).to_hex();
        base_root
            .join(CONTENT_STORE_DIRECTORY)
            .join(app)
            .join(self.scope.to_string())
            .join(self.digest().to_hex())
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn scope(&self) -> SelectedScope {
        self.scope
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn app_id(&self) -> &AppId {
        &self.app_id
    }
}

fn field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}

pub const MAINTENANCE_PACKAGE_NAME: &str = "variant.zup";

pub const MAINTENANCE_RUNTIME_DIRECTORY: &str = "maintenance";

pub const STATE_FOLDER: &str = "zup";

pub const fn scope_name(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

pub fn is_maintenance_path(path: &Path) -> bool {
    if !matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("maintenance" | "maintenance.exe")
    ) {
        return false;
    }
    let Some(instance) = path.parent() else {
        return false;
    };
    instance
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.parse::<zup_core::MaterializationId>().is_ok())
        && instance.parent().is_some_and(|parent| {
            parent.file_name() == Some(std::ffi::OsStr::new("maintenance"))
                && parent
                    .parent()
                    .is_some_and(|root| root.file_name() == Some(std::ffi::OsStr::new(".zup")))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ContentStoreIdentity {
        ContentStoreIdentity::new(
            AppId::new("com.acme.desktop").unwrap(),
            Version::parse("1.4.0").unwrap(),
            SelectedScope::User,
            Target::parse("windows-x64").unwrap(),
            Sha256Digest::from_bytes([7; 32]),
        )
    }

    /// that must not be confused.
    #[test]
    fn an_identity_depends_on_every_field() {
        let base = identity();
        for other in [
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                base.scope(),
                Target::parse("windows-arm64").unwrap(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                base.scope(),
                *base.target(),
                Sha256Digest::from_bytes([8; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                SelectedScope::Machine,
                *base.target(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                Version::parse("1.5.0").unwrap(),
                base.scope(),
                *base.target(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                AppId::new("com.acme.other").unwrap(),
                base.version().clone(),
                base.scope(),
                *base.target(),
                Sha256Digest::from_bytes([7; 32]),
            ),
        ] {
            assert_ne!(base.digest(), other.digest());
        }
    }

    #[test]
    fn fields_are_length_prefixed_so_a_shift_cannot_collide() {
        let base = identity();
        let absorbed = ContentStoreIdentity::new(
            AppId::new("com.acme.desktopx").unwrap(),
            base.version().clone(),
            base.scope(),
            *base.target(),
            Sha256Digest::from_bytes([7; 32]),
        );
        assert_ne!(base.digest(), absorbed.digest());

        let mut joined = Sha256::new();
        field(&mut joined, b"abcd");
        let mut split = Sha256::new();
        field(&mut split, b"ab");
        field(&mut split, b"cd");
        assert_ne!(
            Sha256Digest::from_hasher(joined),
            Sha256Digest::from_hasher(split),
            "an unprefixed field encoding would make these two equal"
        );
    }

    #[test]
    fn the_store_path_nests_app_then_scope_then_identity() {
        let path = identity().path_under(Path::new("/base"));
        let relative = path.strip_prefix("/base").expect("under the base");
        let segments = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(segments.len(), 4, "{segments:?}");
        assert_eq!(segments[0], CONTENT_STORE_DIRECTORY);
        assert_eq!(segments[1].len(), 64, "the application is named by digest");
        assert_eq!(segments[2], "user");
        assert_eq!(segments[3], identity().digest().to_hex());
    }
}
