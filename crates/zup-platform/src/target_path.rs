//! Strongly typed fully-resolved target paths.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Errors produced when constructing a [`TargetPath`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetPathError {
    /// The path is empty.
    #[error("target path must not be empty")]
    Empty,

    /// The path is not absolute.
    #[error("target path `{path}` must be absolute")]
    NotAbsolute { path: String },

    /// The path contains `.` or `..`.
    #[error("target path `{path}` contains `{component}`")]
    Traversal { path: String, component: String },

    /// The path still contains a template variable.
    #[error("target path `{path}` contains an unresolved template variable")]
    UnresolvedVariable { path: String },

    /// The path cannot be represented on the target platform.
    #[error("target path `{path}` is not representable: {reason}")]
    NotRepresentable { path: String, reason: String },
}

/// A fully resolved absolute target-machine path.
///
/// Represents a **desired destination**, not an existing file. Construction
/// does not touch the filesystem and does not canonicalize.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPath(PathBuf);

impl TargetPath {
    /// Validate and wrap a fully resolved absolute path.
    pub fn new(path: PathBuf) -> Result<Self, TargetPathError> {
        if path.as_os_str().is_empty() {
            return Err(TargetPathError::Empty);
        }

        let text = path.to_string_lossy().into_owned();
        if text.contains("${") {
            return Err(TargetPathError::UnresolvedVariable { path: text });
        }
        if !path.is_absolute() {
            return Err(TargetPathError::NotAbsolute { path: text });
        }

        for component in path.components() {
            match component {
                Component::Normal(_) | Component::Prefix(_) | Component::RootDir => {}
                Component::CurDir => {
                    return Err(TargetPathError::Traversal {
                        path: text,
                        component: ".".to_owned(),
                    });
                }
                Component::ParentDir => {
                    return Err(TargetPathError::Traversal {
                        path: text,
                        component: "..".to_owned(),
                    });
                }
            }
        }

        if path.to_str().is_none() {
            return Err(TargetPathError::NotRepresentable {
                path: path.display().to_string(),
                reason: "path is not valid UTF-8".to_owned(),
            });
        }

        Ok(Self(path))
    }

    /// Borrow the underlying OS path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into a `PathBuf`.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for TargetPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for TargetPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Serialize for TargetPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string_lossy())
    }
}

impl<'de> Deserialize<'de> for TargetPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(PathBuf::from(raw)).map_err(serde::de::Error::custom)
    }
}
