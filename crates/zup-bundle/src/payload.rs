//! Portable payload content access.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use zup_core::{PLUGIN_PAYLOAD_ROOT, RelativePath, Sha256Digest, hash_reader};

/// Errors produced while reading payload content.
#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("payload `{path}` is not available")]
    NotFound { path: String },

    #[error("payload `{path}` is not a regular file")]
    NotRegular { path: String },

    #[error("payload `{path}` escapes the payload root")]
    EscapesRoot { path: String },

    #[error("payload `{path}` digest mismatch")]
    DigestMismatch { path: String },

    #[error("payload `{path}` size mismatch: expected {expected}, found {found}")]
    SizeMismatch {
        path: String,
        expected: u64,
        found: u64,
    },

    #[error("failed to read payload `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Opened payload stream.
pub type PayloadReader = Box<dyn Read>;

/// Retrieves payload content by portable identity.
pub trait PayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError>;
}

/// Selects an embedded package for executable paths and a directory for
/// developer workflows. Executable packages are fully verified on creation.
pub enum AutoPayloadSource {
    Directory(DirectoryPayloadSource),
    Bundle(super::BundlePayloadSource),
    Overlay(OverlayPayloadSource),
}

impl AutoPayloadSource {
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, super::BundleError> {
        let path = path.into();
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_file() {
            let bundle = super::EmbeddedBundle::open(&path)?;
            Ok(Self::Bundle(bundle.payload_source()))
        } else {
            Ok(Self::Directory(DirectoryPayloadSource::new(path)))
        }
    }

    pub fn from_paths(
        payload_root: impl Into<PathBuf>,
        payload_overlay_root: Option<PathBuf>,
    ) -> Result<Self, super::BundleError> {
        let base = Self::from_path(payload_root)?;
        Ok(Self::Overlay(OverlayPayloadSource::new(
            base,
            payload_overlay_root,
        )))
    }
}

impl PayloadSource for AutoPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        match self {
            Self::Overlay(source) => source.open(path, expected_sha256, expected_size),
            Self::Directory(source) => {
                if is_plugin_payload_path(path) {
                    return Err(PayloadError::NotFound {
                        path: path.to_string(),
                    });
                }
                source.open(path, expected_sha256, expected_size)
            }
            Self::Bundle(source) => {
                if is_plugin_payload_path(path) {
                    return Err(PayloadError::NotFound {
                        path: path.to_string(),
                    });
                }
                source.open(path, expected_sha256, expected_size)
            }
        }
    }
}

pub struct OverlayPayloadSource {
    base: Box<AutoPayloadSource>,
    overlay: Option<DirectoryPayloadSource>,
}

impl OverlayPayloadSource {
    pub fn new(base: AutoPayloadSource, overlay: Option<PathBuf>) -> Self {
        Self {
            base: Box::new(base),
            overlay: overlay.map(DirectoryPayloadSource::new),
        }
    }
}

impl PayloadSource for OverlayPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        if is_plugin_payload_path(path) {
            return match &self.overlay {
                Some(overlay) => overlay.open(path, expected_sha256, expected_size),
                None => Err(PayloadError::NotFound {
                    path: path.to_string(),
                }),
            };
        }
        if let Some(overlay) = &self.overlay {
            match overlay.open(path, expected_sha256, expected_size) {
                Ok(reader) => return Ok(reader),
                Err(PayloadError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        self.base.open(path, expected_sha256, expected_size)
    }
}

fn is_plugin_payload_path(path: &RelativePath) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.eq_ignore_ascii_case(PLUGIN_PAYLOAD_ROOT))
}

/// Development/build payload provider rooted at a source tree.
pub struct DirectoryPayloadSource {
    root: PathBuf,
}

fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

fn open_payload_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path)
}

impl DirectoryPayloadSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &RelativePath) -> Result<PathBuf, PayloadError> {
        let mut out = self.root.clone();
        for component in path.components() {
            let mut segments = Path::new(component).components();
            let safe = !component.is_empty()
                && !component.contains('/')
                && !component.contains('\\')
                && !component.contains(':')
                && matches!(segments.next(), Some(Component::Normal(_)))
                && segments.next().is_none();
            if !safe {
                return Err(PayloadError::EscapesRoot {
                    path: path.to_string(),
                });
            }
            out.push(component);
        }
        if !out.starts_with(&self.root) {
            return Err(PayloadError::EscapesRoot {
                path: path.to_string(),
            });
        }
        Ok(out)
    }

    fn verify_parent_chain(&self, path: &RelativePath) -> Result<(), PayloadError> {
        let mut current = self.root.clone();
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PayloadError::NotFound {
                    path: path.to_string(),
                }
            } else {
                PayloadError::Read {
                    path: path.to_string(),
                    source: error,
                }
            }
        })?;
        if is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(PayloadError::NotRegular {
                path: path.to_string(),
            });
        }
        for component in path
            .components()
            .take(path.component_count().saturating_sub(1))
        {
            current.push(component);
            let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    PayloadError::NotFound {
                        path: path.to_string(),
                    }
                } else {
                    PayloadError::Read {
                        path: path.to_string(),
                        source: error,
                    }
                }
            })?;
            if is_reparse_point(&metadata) || !metadata.is_dir() {
                return Err(PayloadError::NotRegular {
                    path: path.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl DirectoryPayloadSource {
    fn open_verified(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
        mut after_open: impl FnMut(&Path),
    ) -> Result<PayloadReader, PayloadError> {
        let full = self.resolve(path)?;
        self.verify_parent_chain(path)?;
        #[cfg(unix)]
        {
            let metadata = std::fs::symlink_metadata(&full).map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    PayloadError::NotFound {
                        path: path.to_string(),
                    }
                } else {
                    PayloadError::Read {
                        path: path.to_string(),
                        source,
                    }
                }
            })?;
            if metadata.file_type().is_symlink() {
                return Err(PayloadError::NotRegular {
                    path: path.to_string(),
                });
            }
        }
        let mut file = open_payload_file(&full).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PayloadError::NotFound {
                    path: path.to_string(),
                }
            } else {
                PayloadError::Read {
                    path: path.to_string(),
                    source,
                }
            }
        })?;
        let metadata = file.metadata().map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        if is_reparse_point(&metadata) || !metadata.is_file() {
            return Err(PayloadError::NotRegular {
                path: path.to_string(),
            });
        }
        after_open(&full);
        let (size, digest) = hash_reader(&mut file).map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        if size != expected_size {
            return Err(PayloadError::SizeMismatch {
                path: path.to_string(),
                expected: expected_size,
                found: size,
            });
        }
        if digest != *expected_sha256 {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|source| PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        Ok(Box::new(file))
    }
}

impl PayloadSource for DirectoryPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        self.open_verified(path, expected_sha256, expected_size, |_| {})
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn returns_the_verified_handle_after_the_path_is_replaced() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("payload.bin");
        let replacement = root.path().join("replacement.bin");
        std::fs::write(&path, b"original").unwrap();
        let source = DirectoryPayloadSource::new(root.path());
        let relative = RelativePath::new("payload.bin").unwrap();
        let digest = hash_reader(&b"original"[..]).unwrap().1;
        let mut reader = source
            .open_verified(&relative, &digest, 8, |opened| {
                std::fs::rename(opened, &replacement).unwrap();
                std::fs::write(opened, b"replaced").unwrap();
            })
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }
}
