//! Portable relative paths for logical payload identity.

use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Errors produced when constructing a [`RelativePath`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RelativePathError {
    /// The path is empty or has no components.
    #[error("relative path must not be empty")]
    Empty,

    /// The path is absolute or has a prefix/root.
    #[error("path `{path}` must be relative")]
    Absolute { path: String },

    /// The path contains a `..` component.
    #[error("path `{path}` must not contain `..`")]
    ParentTraversal { path: String },

    /// A component is empty, `.`, or otherwise not a single portable segment.
    #[error("path `{path}` contains an invalid component `{component}`")]
    InvalidComponent { path: String, component: String },

    /// The path is not valid UTF-8 for portable representation.
    #[error("path `{path}` is not valid UTF-8")]
    NotUtf8 { path: String },
}

/// A non-empty, relative, `/`-separated logical path.
///
/// This is portable payload identity: build-machine paths stay in `PathBuf`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativePath {
    inner: String,
}

impl RelativePath {
    /// Parse a portable relative path using `/` or `\` as separators.
    pub fn new(value: impl AsRef<str>) -> Result<Self, RelativePathError> {
        let raw = value.as_ref();
        if raw.is_empty() {
            return Err(RelativePathError::Empty);
        }

        let normalized = raw.replace('\\', "/");
        if normalized.starts_with('/') {
            return Err(RelativePathError::Absolute {
                path: raw.to_owned(),
            });
        }

        let mut parts = Vec::new();
        for part in normalized.split('/') {
            if part.is_empty() || part == "." {
                return Err(RelativePathError::InvalidComponent {
                    path: raw.to_owned(),
                    component: part.to_owned(),
                });
            }
            if part == ".." {
                return Err(RelativePathError::ParentTraversal {
                    path: raw.to_owned(),
                });
            }
            if part.contains('\0') {
                return Err(RelativePathError::InvalidComponent {
                    path: raw.to_owned(),
                    component: part.to_owned(),
                });
            }
            parts.push(part);
        }

        if parts.is_empty() {
            return Err(RelativePathError::Empty);
        }

        Ok(Self {
            inner: parts.join("/"),
        })
    }

    /// Convert a build-machine path into portable form, if safe.
    pub fn from_path(path: &Path) -> Result<Self, RelativePathError> {
        if path.as_os_str().is_empty() {
            return Err(RelativePathError::Empty);
        }
        if path.is_absolute() {
            return Err(RelativePathError::Absolute {
                path: path.display().to_string(),
            });
        }

        let text = path.to_str().ok_or_else(|| RelativePathError::NotUtf8 {
            path: path.display().to_string(),
        })?;

        // Disassemble via components so prefixes/`..` are rejected structurally.
        for component in path.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {
                    return Err(RelativePathError::InvalidComponent {
                        path: text.to_owned(),
                        component: ".".to_owned(),
                    });
                }
                Component::ParentDir => {
                    return Err(RelativePathError::ParentTraversal {
                        path: text.to_owned(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(RelativePathError::Absolute {
                        path: text.to_owned(),
                    });
                }
            }
        }

        Self::new(text)
    }

    /// Build from already-validated `/`-separated components.
    pub fn from_components<I, S>(components: I) -> Result<Self, RelativePathError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let joined = components
            .into_iter()
            .map(|c| c.as_ref().to_owned())
            .collect::<Vec<_>>()
            .join("/");
        Self::new(joined)
    }

    /// The canonical `/`-separated form.
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Split into path components.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.inner.split('/')
    }

    /// Number of path components.
    pub fn component_count(&self) -> usize {
        self.inner.split('/').count()
    }

    /// Final component.
    pub fn file_name(&self) -> &str {
        self.inner
            .rsplit('/')
            .next()
            .expect("relative path always has a component")
    }

    /// Path without the final component, if any.
    pub fn parent(&self) -> Option<RelativePath> {
        let (parent, _) = self.inner.rsplit_once('/')?;
        Self::new(parent).ok()
    }

    /// Append another relative path.
    pub fn join(&self, other: &RelativePath) -> RelativePath {
        RelativePath {
            inner: format!("{}/{}", self.inner, other.inner),
        }
    }

    /// Append a single validated component.
    pub fn push(&mut self, component: &str) -> Result<(), RelativePathError> {
        let next = match self.inner.is_empty() {
            true => component.to_owned(),
            false => format!("{}/{component}", self.inner),
        };
        *self = Self::new(next)?;
        Ok(())
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

impl Serialize for RelativePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.inner)
    }
}

impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for RelativePath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelativePath".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A `/`-separated path relative to the project directory. \
                            Absolute paths and `..` are refused.",
        })
    }
}
