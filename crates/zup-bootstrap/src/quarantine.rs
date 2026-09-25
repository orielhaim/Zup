use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{PrerequisiteId, RelativePath, Sha256Digest};

use crate::model::QuarantinedArtifact;

#[derive(Debug, Error)]
pub enum QuarantineError {
    #[error("quarantine path is outside its root")]
    OutsideRoot,
    #[error("unsafe prerequisite filename `{0}`")]
    UnsafeFilename(String),
    #[error("quarantine I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("quarantine artifact size mismatch: expected {expected}, found {found}")]
    SizeMismatch { expected: u64, found: u64 },
    #[error("quarantine artifact digest mismatch")]
    DigestMismatch,
    #[error("quarantine reservation is incomplete")]
    Incomplete,
    #[error("quarantine artifact exceeds the package limit")]
    TooLarge,
}

pub fn validate_filename(filename: &str) -> Result<(), QuarantineError> {
    let base = filename.rsplit(['\\', '/']).next().unwrap_or(filename);
    let valid = base == filename
        && !base.is_empty()
        && base.len() <= 255
        && !base.contains([':', '\0'])
        && base != "."
        && base != ".."
        && !base.ends_with(['.', ' '])
        && !base.chars().any(char::is_control)
        && !matches!(
            base.split('.')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
    if valid {
        Ok(())
    } else {
        Err(QuarantineError::UnsafeFilename(filename.to_owned()))
    }
}

pub struct Quarantine {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactReservation {
    pub relative_path: RelativePath,
    pub partial_path: PathBuf,
    pub final_path: PathBuf,
    pub expected_size: Option<u64>,
}

impl Quarantine {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, QuarantineError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| QuarantineError::Io {
            path: root.clone(),
            source,
        })?;
        reject_reparse_points(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn reserve(
        &self,
        id: &PrerequisiteId,
        filename: &str,
        expected_size: Option<u64>,
    ) -> Result<ArtifactReservation, QuarantineError> {
        validate_filename(filename)?;
        if expected_size.is_some_and(|size| size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES) {
            return Err(QuarantineError::TooLarge);
        }
        let relative_path = RelativePath::from_components([id.as_str(), filename])
            .map_err(|_| QuarantineError::UnsafeFilename(filename.to_owned()))?;
        let final_path = self.root.join(relative_path.as_str());
        let partial_path = final_path.with_extension("partial");
        let parent = final_path.parent().ok_or(QuarantineError::OutsideRoot)?;
        fs::create_dir_all(parent).map_err(|source| QuarantineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        reject_reparse_points(&self.root)?;
        reject_reparse_points(parent)?;
        Ok(ArtifactReservation {
            relative_path,
            partial_path,
            final_path,
            expected_size,
        })
    }

    pub fn stage_bytes(
        &self,
        reservation: &ArtifactReservation,
        bytes: &[u8],
        expected_digest: Sha256Digest,
    ) -> Result<QuarantinedArtifact, QuarantineError> {
        self.stage_reader(reservation, bytes, expected_digest)
    }

    pub fn stage_reader<R: Read>(
        &self,
        reservation: &ArtifactReservation,
        mut reader: R,
        expected_digest: Sha256Digest,
    ) -> Result<QuarantinedArtifact, QuarantineError> {
        reject_reparse_points(&reservation.partial_path)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&reservation.partial_path)
            .map_err(|source| QuarantineError::Io {
                path: reservation.partial_path.clone(),
                source,
            })?;
        let result = (|| {
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|source| QuarantineError::Io {
                        path: reservation.partial_path.clone(),
                        source,
                    })?;
                if read == 0 {
                    break;
                }
                size = size
                    .checked_add(read as u64)
                    .ok_or(QuarantineError::TooLarge)?;
                if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
                    return Err(QuarantineError::TooLarge);
                }
                if reservation
                    .expected_size
                    .is_some_and(|expected| size > expected)
                {
                    return Err(QuarantineError::SizeMismatch {
                        expected: reservation.expected_size.unwrap_or_default(),
                        found: size,
                    });
                }
                file.write_all(&buffer[..read])
                    .map_err(|source| QuarantineError::Io {
                        path: reservation.partial_path.clone(),
                        source,
                    })?;
            }
            if let Some(expected) = reservation.expected_size
                && expected != size
            {
                return Err(QuarantineError::SizeMismatch {
                    expected,
                    found: size,
                });
            }
            file.flush()
                .and_then(|_| file.sync_all())
                .map_err(|source| QuarantineError::Io {
                    path: reservation.partial_path.clone(),
                    source,
                })
        })();
        drop(file);
        if let Err(error) = result {
            let _ = fs::remove_file(&reservation.partial_path);
            return Err(error);
        }
        self.publish(reservation, expected_digest)
    }

    pub fn publish(
        &self,
        reservation: &ArtifactReservation,
        expected_digest: Sha256Digest,
    ) -> Result<QuarantinedArtifact, QuarantineError> {
        reject_reparse_points(&reservation.partial_path)?;
        let (size, digest) = hash_file(&reservation.partial_path)?;
        if let Some(expected_size) = reservation.expected_size
            && size != expected_size
        {
            let _ = fs::remove_file(&reservation.partial_path);
            return Err(QuarantineError::SizeMismatch {
                expected: expected_size,
                found: size,
            });
        }
        if digest != expected_digest {
            let _ = fs::remove_file(&reservation.partial_path);
            return Err(QuarantineError::DigestMismatch);
        }
        if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
            let _ = fs::remove_file(&reservation.partial_path);
            return Err(QuarantineError::TooLarge);
        }
        publish_replace(&reservation.partial_path, &reservation.final_path).map_err(|source| {
            QuarantineError::Io {
                path: reservation.final_path.clone(),
                source,
            }
        })?;
        Ok(QuarantinedArtifact {
            relative_path: reservation.relative_path.clone(),
            size,
            sha256: digest,
        })
    }

    pub fn verify(&self, artifact: &QuarantinedArtifact) -> Result<(), QuarantineError> {
        let path = self.resolve(&artifact.relative_path)?;
        let metadata = fs::symlink_metadata(&path).map_err(|source| QuarantineError::Io {
            path: path.clone(),
            source,
        })?;
        if is_reparse_point(&metadata) || !metadata.is_file() {
            return Err(QuarantineError::OutsideRoot);
        }
        let (size, digest) = hash_file(&path)?;
        if size != artifact.size || digest != artifact.sha256 {
            return Err(QuarantineError::DigestMismatch);
        }
        Ok(())
    }

    pub fn resolve(&self, relative_path: &RelativePath) -> Result<PathBuf, QuarantineError> {
        let path = self.root.join(relative_path.as_str());
        if !path.starts_with(&self.root) || path == self.root {
            return Err(QuarantineError::OutsideRoot);
        }
        reject_reparse_points(&path)?;
        Ok(path)
    }

    pub fn remove_partial(&self, reservation: &ArtifactReservation) -> Result<(), QuarantineError> {
        match fs::remove_file(&reservation.partial_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(QuarantineError::Io {
                path: reservation.partial_path.clone(),
                source,
            }),
        }
    }

    pub fn remove_artifact(&self, artifact: &QuarantinedArtifact) -> Result<(), QuarantineError> {
        let path = self.resolve(&artifact.relative_path)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(QuarantineError::Io { path, source }),
        }
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

fn reject_reparse_points(path: &Path) -> Result<(), QuarantineError> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| QuarantineError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let mut components = Vec::new();
    while let Some(parent) = current.parent() {
        components.push(parent.to_path_buf());
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    components.reverse();
    components.push(path.to_path_buf());
    for component in components {
        match fs::symlink_metadata(&component) {
            Ok(metadata) if is_reparse_point(&metadata) => {
                return Err(QuarantineError::OutsideRoot);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(QuarantineError::Io {
                    path: component,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, Sha256Digest), QuarantineError> {
    let mut file = File::open(path).map_err(|source| QuarantineError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| QuarantineError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or(QuarantineError::TooLarge)?;
        if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
            return Err(QuarantineError::TooLarge);
        }
        hasher.update(&buffer[..read]);
    }
    Ok((size, Sha256Digest::from_bytes(hasher.finalize().into())))
}
