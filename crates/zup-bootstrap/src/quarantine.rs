use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{PrerequisiteId, RelativePath, Sha256Digest};

use crate::filesystem::{BootstrapFileSystem, PortableBootstrapFileSystem};
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
    file_system: Arc<dyn BootstrapFileSystem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactReservation {
    pub relative_path: RelativePath,
    pub partial_path: PathBuf,
    pub final_path: PathBuf,
    pub expected_size: Option<u64>,
}

impl Quarantine {
    /// Quarantine on the portable `std::fs` filesystem.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, QuarantineError> {
        Self::with_file_system(root, Arc::new(PortableBootstrapFileSystem))
    }

    /// Quarantine that publishes and clears links through `file_system`, for
    /// hosts that must guarantee more than `std::fs` can.
    pub fn with_file_system(
        root: impl Into<PathBuf>,
        file_system: Arc<dyn BootstrapFileSystem>,
    ) -> Result<Self, QuarantineError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| QuarantineError::Io {
            path: root.clone(),
            source,
        })?;
        let quarantine = Self { root, file_system };
        quarantine.reject_links(quarantine.root.as_path())?;
        Ok(quarantine)
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
        self.reject_links(&self.root)?;
        self.reject_links(parent)?;
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
        self.reject_links(&reservation.partial_path)?;
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
        self.reject_links(&reservation.partial_path)?;
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
        self.file_system
            .publish_replace(&reservation.partial_path, &reservation.final_path)
            .map_err(|source| QuarantineError::Io {
                path: reservation.final_path.clone(),
                source,
            })?;
        Ok(QuarantinedArtifact {
            relative_path: reservation.relative_path.clone(),
            size,
            sha256: digest,
        })
    }

    pub fn verify(&self, artifact: &QuarantinedArtifact) -> Result<(), QuarantineError> {
        let path = self.resolve(&artifact.relative_path)?;
        if self.is_link(&path)? {
            return Err(QuarantineError::OutsideRoot);
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| QuarantineError::Io {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
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
        self.reject_links(&path)?;
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

    /// Refuse to proceed when any component of `path`, or `path` itself, is a
    /// link: staging or reading through one would move data outside the root.
    fn reject_links(&self, path: &Path) -> Result<(), QuarantineError> {
        for component in link_checked_prefixes(path)? {
            if self.is_link(&component)? {
                return Err(QuarantineError::OutsideRoot);
            }
        }
        Ok(())
    }

    fn is_link(&self, path: &Path) -> Result<bool, QuarantineError> {
        self.file_system
            .is_link(path)
            .map_err(|source| QuarantineError::Io {
                path: path.to_path_buf(),
                source,
            })
    }
}

/// `path` preceded by each of its ancestors, from the filesystem root down, so
/// a link anywhere along the way is inspected before the leaf is touched.
fn link_checked_prefixes(path: &Path) -> Result<Vec<PathBuf>, QuarantineError> {
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
    Ok(components)
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
