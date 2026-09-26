use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zup_core::{AppId, SelectedScope, Sha256Digest, TargetTriple};

use crate::filesystem::{BootstrapFileSystem, PortableBootstrapFileSystem};
use crate::model::{BootstrapId, BootstrapState, MAX_BOOTSTRAP_STATE_BYTES};

#[derive(Debug, Error)]
pub enum BootstrapStoreError {
    #[error("bootstrap state I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("bootstrap state JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bootstrap state is missing")]
    Missing,
    #[error("bootstrap state already exists")]
    AlreadyExists,
    #[error("bootstrap state revision conflict")]
    RevisionConflict,
    #[error("bootstrap state failed integrity or bounds validation")]
    Invalid,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StateEnvelope {
    schema: u32,
    payload: BootstrapState,
    sha256: Sha256Digest,
}

pub trait BootstrapStateStore {
    fn create(&self, state: &BootstrapState) -> Result<(), BootstrapStoreError>;
    fn load(&self, id: BootstrapId) -> Result<BootstrapState, BootstrapStoreError>;
    fn compare_and_swap(
        &self,
        expected_revision: u64,
        state: &BootstrapState,
    ) -> Result<(), BootstrapStoreError>;
    fn find_resumable(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<Vec<BootstrapState>, BootstrapStoreError>;
    fn remove(&self, id: BootstrapId) -> Result<(), BootstrapStoreError>;
}

#[derive(Clone)]
pub struct FilesystemBootstrapStateStore {
    root: PathBuf,
    file_system: Arc<dyn BootstrapFileSystem>,
}

impl FilesystemBootstrapStateStore {
    /// Store backed by the portable `std::fs` filesystem.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_file_system(root, Arc::new(PortableBootstrapFileSystem))
    }

    /// Store that publishes state and clears links through `file_system`, for
    /// hosts that must guarantee more than `std::fs` can.
    pub fn with_file_system(
        root: impl Into<PathBuf>,
        file_system: Arc<dyn BootstrapFileSystem>,
    ) -> Self {
        Self {
            root: root.into(),
            file_system,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Refuse to read or publish state through a link: a link at the root or at
    /// the state file would move bootstrap state outside its own root.
    fn reject_links(&self, path: &Path) -> Result<(), BootstrapStoreError> {
        match self.file_system.is_link(path) {
            Ok(true) => Err(BootstrapStoreError::Invalid),
            Ok(false) => Ok(()),
            Err(source) => Err(BootstrapStoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn state_path(&self, id: BootstrapId) -> PathBuf {
        self.root
            .join("bootstrap")
            .join(id.as_uuid().to_string())
            .join("state.json")
    }

    fn write_state(&self, path: &Path, state: &BootstrapState) -> Result<(), BootstrapStoreError> {
        self.reject_links(&self.root)?;
        self.reject_links(path)?;
        if state.operations.len() > crate::model::MAX_BOOTSTRAP_OPERATIONS {
            return Err(BootstrapStoreError::Invalid);
        }
        let payload = serde_json::to_vec(state)?;
        if payload.len() as u64 > MAX_BOOTSTRAP_STATE_BYTES {
            return Err(BootstrapStoreError::Invalid);
        }
        let envelope = StateEnvelope {
            schema: state.schema,
            payload: state.clone(),
            sha256: Sha256Digest::from_bytes(Sha256::digest(&payload).into()),
        };
        let bytes = serde_json::to_vec(&envelope)?;
        if bytes.len() as u64 > MAX_BOOTSTRAP_STATE_BYTES {
            return Err(BootstrapStoreError::Invalid);
        }
        let parent = path.parent().ok_or(BootstrapStoreError::Invalid)?;
        fs::create_dir_all(parent).map_err(|source| BootstrapStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        self.reject_links(&self.root)?;
        let temporary = parent.join(format!(".state-{}.partial", Uuid::now_v7()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| BootstrapStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        let result = (|| {
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        drop(file);
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary);
            return Err(BootstrapStoreError::Io {
                path: temporary,
                source,
            });
        }
        self.file_system
            .publish_replace(&temporary, path)
            .map_err(|source| {
                let _ = fs::remove_file(&temporary);
                BootstrapStoreError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        Ok(())
    }

    fn read_state(&self, path: &Path) -> Result<BootstrapState, BootstrapStoreError> {
        self.reject_links(path)?;
        let metadata = fs::metadata(path).map_err(|source| BootstrapStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.len() > MAX_BOOTSTRAP_STATE_BYTES {
            return Err(BootstrapStoreError::Invalid);
        }
        let mut file = File::open(path).map_err(|source| BootstrapStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| BootstrapStoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let envelope: StateEnvelope = serde_json::from_slice(&bytes)?;
        if envelope.schema != crate::model::BOOTSTRAP_STATE_SCHEMA {
            return Err(BootstrapStoreError::Invalid);
        }
        let payload = serde_json::to_vec(&envelope.payload)?;
        if Sha256Digest::from_bytes(Sha256::digest(&payload).into()) != envelope.sha256 {
            return Err(BootstrapStoreError::Invalid);
        }
        if envelope.payload.schema != crate::model::BOOTSTRAP_STATE_SCHEMA
            || envelope.payload.operations.len() > crate::model::MAX_BOOTSTRAP_OPERATIONS
        {
            return Err(BootstrapStoreError::Invalid);
        }
        Ok(envelope.payload)
    }
}

impl BootstrapStateStore for FilesystemBootstrapStateStore {
    fn create(&self, state: &BootstrapState) -> Result<(), BootstrapStoreError> {
        let path = self.state_path(state.id);
        self.reject_links(&path)?;
        if path.exists() {
            return Err(BootstrapStoreError::AlreadyExists);
        }
        self.write_state(&path, state)
    }

    fn load(&self, id: BootstrapId) -> Result<BootstrapState, BootstrapStoreError> {
        let path = self.state_path(id);
        if !path.exists() {
            return Err(BootstrapStoreError::Missing);
        }
        self.read_state(&path)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        state: &BootstrapState,
    ) -> Result<(), BootstrapStoreError> {
        let path = self.state_path(state.id);
        let current = self.read_state(&path)?;
        if current.revision != expected_revision || state.revision != expected_revision + 1 {
            return Err(BootstrapStoreError::RevisionConflict);
        }
        self.write_state(&path, state)
    }

    fn find_resumable(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<Vec<BootstrapState>, BootstrapStoreError> {
        let root = self.root.join("bootstrap");
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(BootstrapStoreError::Io { path: root, source });
            }
        };
        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| BootstrapStoreError::Io {
                path: root.clone(),
                source,
            })?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
                .map(BootstrapId::from_uuid)
            else {
                continue;
            };
            if let Ok(state) = self.read_state(&self.state_path(id))
                && state.key.app_id == *app_id
                && state.key.scope == scope
                && &state.key.target == target
            {
                states.push(state);
            }
        }
        states.sort_by_key(|state| state.id.as_uuid());
        Ok(states)
    }

    fn remove(&self, id: BootstrapId) -> Result<(), BootstrapStoreError> {
        let path = self.root.join("bootstrap").join(id.as_uuid().to_string());
        match fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(BootstrapStoreError::Io { path, source }),
        }
    }
}
