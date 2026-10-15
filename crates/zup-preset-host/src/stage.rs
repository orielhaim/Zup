use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("staged content does not match the verified preset")]
    Tampered,
    #[error("the staging path is not a regular file")]
    Redirected,
    #[error("{0}")]
    Io(String),
}

pub fn confirm_bytes(path: &Path, expected: &[u8]) -> Result<(), StageError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| StageError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StageError::Redirected);
    }
    let actual = std::fs::read(path).map_err(|error| StageError::Io(error.to_string()))?;
    if actual != expected {
        return Err(StageError::Tampered);
    }
    Ok(())
}

pub struct PresetStage {
    root: PathBuf,
}

impl PresetStage {
    pub fn acquire(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for PresetStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
