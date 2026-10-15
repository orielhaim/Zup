use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use private_fs::Publication;

use crate::id::TransactionId;
use crate::private;
use crate::record::{CorruptReason, PublishError, StoreError, TransactionRecord};

/// Directory capability that already carries its security policy.
///
/// The transaction coordinator never opens a raw path and decides the policy
/// itself. Callers at the trust boundary choose a private volume or a
/// platform machine volume.
pub trait JournalVolume: Send + Sync {
    fn read_journal(&self, id: &TransactionId) -> Result<Option<Vec<u8>>, StoreError>;
    fn publish_journal(
        &self,
        id: &TransactionId,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<(), StoreError>;
    fn lock_journal(&self, id: &TransactionId) -> Result<File, StoreError>;
    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError>;
}

const MAX_UPDATE_ATTEMPTS: u32 = 8;

pub trait TransactionStore {
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
        self.volume.list_journals()
    }
}

struct PrivateJournals {
    root: PathBuf,
}

impl PrivateJournals {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn transactions(&self) -> Result<private_fs::PrivateDir, StoreError> {
        let path = self.root.join("transactions");
        private::ensure_dir(&path).map_err(|error| store_fs(path, error))
    }

    fn journal(&self, id: &TransactionId) -> Result<private_fs::PrivateDir, StoreError> {
        let name = id.to_string();
        let path = self.root.join("transactions").join(&name);
        let root = self.transactions()?;
        root.ensure_dir(&name)
            .map_err(|error| store_fs(path.clone(), error))?;
        root.sync_all()
            .map_err(|error| store_fs(path.clone(), error))?;
        private_fs::PrivateDir::open(&path).map_err(|error| store_fs(path, error))
    }
}

impl JournalVolume for PrivateJournals {
    fn read_journal(&self, id: &TransactionId) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.root.join("transactions").join(id.to_string());
        let directory = match private_fs::PrivateDir::open(&path) {
            Ok(directory) => directory,
            Err(private_fs::Error::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(store_fs(path, error)),
        };
        match private::read_file(&directory, "transaction.json") {
            Ok(bytes) => Ok(Some(bytes)),
            Err(private_fs::Error::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(store_fs(path.join("transaction.json"), error)),
        }
    }

    fn publish_journal(
        &self,
        id: &TransactionId,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<(), StoreError> {
        let directory = self.journal(id)?;
        let path = self
            .root
            .join("transactions")
            .join(id.to_string())
            .join("transaction.json");
        let policy = if create_new {
            Publication::CreateNew
        } else {
            Publication::Replace
        };
        private::publish_full(&directory, "transaction.json", bytes, policy)
            .map_err(|source| StoreError::Publication { path, source })
    }

    fn lock_journal(&self, id: &TransactionId) -> Result<File, StoreError> {
        let directory = self.journal(id)?;
        let path = self
            .root
            .join("transactions")
            .join(id.to_string())
            .join("transaction.lock");
        let file = private::open_lock(&directory, "transaction.lock")
            .map_err(|source| store_fs(path.clone(), source))?;
        file.lock()
            .map_err(|source| StoreError::Io { path, source })?;
        Ok(file)
    }

    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError> {
        let path = self.root.join("transactions");
        let directory = match private_fs::PrivateDir::open(&path) {
            Ok(directory) => directory,
            Err(private_fs::Error::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(store_fs(path, error)),
        };
        let mut ids = Vec::new();
        for name in directory
            .entries()
            .map_err(|error| store_fs(self.root.join("transactions"), error))?
        {
            let Ok(id) = name.to_string_lossy().parse::<TransactionId>() else {
                continue;
            };
            ids.push(id);
        }
        Ok(ids)
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
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        record.validate()?;
        let id = record.transaction_id;
        let _lock = self.volume.lock_journal(&id)?;
        if self.volume.read_journal(&id)?.is_some() {
            return Err(StoreError::AlreadyExists { id: id.to_string() });
        }
        let bytes = serde_json::to_vec_pretty(record).map_err(StoreError::Serialize)?;
        match self.volume.publish_journal(&id, &bytes, true) {
            Ok(()) => Ok(()),
            Err(StoreError::Publication { source, .. }) if private::absent_collision(&source) => {
                Err(StoreError::AlreadyExists { id: id.to_string() })
            }
            Err(error) => Err(error),
        }
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        match self.volume.read_journal(id)? {
            Some(bytes) => decode_record(&bytes),
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
        let _lock = self.volume.lock_journal(&id)?;
        let current = self.load(&id)?;
        if current.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
            });
        }
        let bytes = serde_json::to_vec_pretty(updated).map_err(StoreError::Serialize)?;
        self.volume.publish_journal(&id, &bytes, false)
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

const MAINTENANCE_DIRECTORY: &str = "maintenance";

pub const MAINTENANCE_PACKAGE_NAME: &str = "variant.zup";

pub const MAINTENANCE_INDEX_NAME: &str = "artifact.json";

pub const MAINTENANCE_RUNTIME_DIRECTORY: &str = "maintenance";

pub const STATE_FOLDER: &str = "zup";

pub const fn scope_name(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

pub fn maintenance_root(state_root: &Path, app_id: &AppId, scope: SelectedScope) -> PathBuf {
    state_root
        .join(MAINTENANCE_DIRECTORY)
        .join(app_id.as_str())
        .join(scope_name(scope))
}

/// version that was never checked - and the directory name is what tells one
pub fn maintenance_directory(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &Version,
) -> PathBuf {
    maintenance_root(state_root, app_id, scope).join(version.to_string())
}

pub fn maintenance_runtime_path(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &Version,
    executable_suffix: &str,
) -> PathBuf {
    maintenance_directory(state_root, app_id, scope, version).join(format!(
        "{MAINTENANCE_RUNTIME_DIRECTORY}{executable_suffix}"
    ))
}

pub fn is_maintenance_path(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == MAINTENANCE_DIRECTORY && pair[1] != MAINTENANCE_DIRECTORY)
}

#[cfg(test)]
mod installed_tests {
    use super::*;
    use semver::Version;
    use std::path::Path;
    use zup_core::{AppId, SelectedScope, Target};

    fn app_id() -> AppId {
        AppId::new("com.acme.desktop").expect("a valid id")
    }

    fn target() -> Target {
        Target::parse("windows-x64").expect("a target")
    }

    fn version(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    /// directories - which is why they must not share a directory.
    #[test]
    fn the_scopes_never_share_a_maintenance_root() {
        let state = Path::new("/var/lib/zup");
        let user = maintenance_root(state, &app_id(), SelectedScope::User);
        let machine = maintenance_root(state, &app_id(), SelectedScope::Machine);
        assert_ne!(user, machine);
        assert!(user.ends_with("user"));
        assert!(machine.ends_with("machine"));
        assert_eq!(
            user.parent().and_then(Path::file_name),
            Some(app_id().as_str().as_ref()),
            "both nest under the application, so a reader can ask about one app across scopes"
        );
    }

    #[test]
    fn a_generation_is_one_version_below_the_root() {
        let state = Path::new("/var/lib/zup");
        let root = maintenance_root(state, &app_id(), SelectedScope::User);
        let one = maintenance_directory(state, &app_id(), SelectedScope::User, &version("1.4.0"));
        let two = maintenance_directory(state, &app_id(), SelectedScope::User, &version("1.5.0"));
        assert!(one.starts_with(&root));
        assert!(two.starts_with(&root));
        assert_ne!(
            one, two,
            "two generations are two directories while both are owned"
        );
    }

    #[test]
    fn the_persisted_runtime_is_named_by_the_targets_convention() {
        let state = Path::new("/var/lib/zup");
        let windows = maintenance_runtime_path(
            state,
            &app_id(),
            SelectedScope::User,
            &version("1.4.0"),
            target().executable_suffix(),
        );
        let linux = maintenance_runtime_path(
            state,
            &app_id(),
            SelectedScope::User,
            &version("1.4.0"),
            Target::parse("linux-x64")
                .expect("a target")
                .executable_suffix(),
        );
        assert_eq!(
            windows.file_name().and_then(|n| n.to_str()),
            Some("maintenance.exe")
        );
        assert_eq!(
            linux.file_name().and_then(|n| n.to_str()),
            Some("maintenance")
        );
    }

    #[test]
    fn a_persisted_runtime_is_recognized_by_where_it_lives() {
        let persisted = Path::new("zup")
            .join(MAINTENANCE_DIRECTORY)
            .join(app_id().as_str())
            .join("user")
            .join("1.4.0")
            .join(format!("{MAINTENANCE_RUNTIME_DIRECTORY}.exe"));
        assert!(is_maintenance_path(&persisted));

        assert!(
            !is_maintenance_path(
                &Path::new("Downloads").join(format!("{MAINTENANCE_RUNTIME_DIRECTORY}-Setup.exe"))
            ),
            "a file outside any maintenance directory is an installation medium, not a runtime"
        );
        assert!(
            !is_maintenance_path(&Path::new("zup").join(MAINTENANCE_DIRECTORY)),
            "the maintenance directory itself is not a generation's runtime"
        );
    }

    #[test]
    fn the_sidecar_documents_carry_no_executable_suffix() {
        for name in [MAINTENANCE_PACKAGE_NAME, MAINTENANCE_INDEX_NAME] {
            assert!(
                !name.ends_with(".exe"),
                "{name} is a document, not a program"
            );
        }
    }
}
