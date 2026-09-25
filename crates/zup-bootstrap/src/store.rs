use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zup_core::{AppId, SelectedScope, Sha256Digest};

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
    ) -> Result<Vec<BootstrapState>, BootstrapStoreError>;
    fn remove(&self, id: BootstrapId) -> Result<(), BootstrapStoreError>;
}

#[derive(Clone)]
pub struct FilesystemBootstrapStateStore {
    root: PathBuf,
}

impl FilesystemBootstrapStateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state_path(&self, id: BootstrapId) -> PathBuf {
        self.root
            .join("bootstrap")
            .join(id.as_uuid().to_string())
            .join("state.json")
    }

    fn write_state(&self, path: &Path, state: &BootstrapState) -> Result<(), BootstrapStoreError> {
        reject_reparse_points(&self.root)?;
        reject_reparse_points(path)?;
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
        reject_reparse_points(&self.root)?;
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
        publish_replace(&temporary, path).map_err(|source| {
            let _ = fs::remove_file(&temporary);
            BootstrapStoreError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Ok(())
    }

    fn read_state(&self, path: &Path) -> Result<BootstrapState, BootstrapStoreError> {
        reject_reparse_points(path)?;
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

fn publish_replace(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let from = from
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let to = to
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        unsafe {
            MoveFileExW(
                windows::core::PCWSTR(from.as_ptr()),
                windows::core::PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| std::io::Error::other(error.to_string()))
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn reject_reparse_points(path: &Path) -> Result<(), BootstrapStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_reparse_point(&metadata) => Err(BootstrapStoreError::Invalid),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BootstrapStoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

impl BootstrapStateStore for FilesystemBootstrapStateStore {
    fn create(&self, state: &BootstrapState) -> Result<(), BootstrapStoreError> {
        let path = self.state_path(state.id);
        reject_reparse_points(&path)?;
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
