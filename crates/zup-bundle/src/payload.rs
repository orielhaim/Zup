//! Portable payload content access.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use thiserror::Error;
use zup_core::{RelativePath, Sha256Digest, hash_reader};

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
}

impl PayloadSource for AutoPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        match self {
            Self::Directory(source) => source.open(path, expected_sha256, expected_size),
            Self::Bundle(source) => source.open(path, expected_sha256, expected_size),
        }
    }
}

/// Development/build payload provider rooted at a source tree.
pub struct DirectoryPayloadSource {
    root: PathBuf,
}

impl DirectoryPayloadSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &RelativePath) -> Result<PathBuf, PayloadError> {
        let mut out = self.root.clone();
        for component in path.components() {
            if component.is_empty()
                || component == "."
                || component == ".."
                || component.contains('\\')
            {
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
}

impl PayloadSource for DirectoryPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        let full = self.resolve(path)?;
        let meta = std::fs::symlink_metadata(&full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PayloadError::NotFound {
                    path: path.to_string(),
                }
            } else {
                PayloadError::Read {
                    path: path.to_string(),
                    source: e,
                }
            }
        })?;
        let ft = meta.file_type();
        if ft.is_symlink() || !ft.is_file() {
            return Err(PayloadError::NotRegular {
                path: path.to_string(),
            });
        }

        let file = File::open(&full).map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        let (size, digest) = hash_reader(file).map_err(|source| PayloadError::Read {
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

        let file = File::open(&full).map_err(|source| PayloadError::Read {
            path: path.to_string(),
            source,
        })?;
        Ok(Box::new(file))
    }
}
