use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RelativePathError {
    #[error("relative path must not be empty")]
    Empty,

    #[error("path `{path}` must be relative")]
    Absolute { path: String },

    #[error("path `{path}` must not contain `..`")]
    ParentTraversal { path: String },

    #[error("path `{path}` contains an invalid component `{component}`")]
    InvalidComponent { path: String, component: String },

    #[error("path `{path}` is not valid UTF-8")]
    NotUtf8 { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativePath {
    inner: String,
}

impl RelativePath {
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

    pub fn as_str(&self) -> &str {
        &self.inner
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.inner.split('/')
    }

    pub fn component_count(&self) -> usize {
        self.inner.split('/').count()
    }

    pub fn file_name(&self) -> &str {
        self.inner
            .rsplit('/')
            .next()
            .expect("relative path always has a component")
    }

    pub fn parent(&self) -> Option<RelativePath> {
        let (parent, _) = self.inner.rsplit_once('/')?;
        Self::new(parent).ok()
    }

    pub fn join(&self, other: &RelativePath) -> RelativePath {
        RelativePath {
            inner: format!("{}/{}", self.inner, other.inner),
        }
    }

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectPath(String);

impl ProjectPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, RelativePathError> {
        let raw = value.as_ref();
        if raw.trim().is_empty() {
            return Err(RelativePathError::Empty);
        }
        let normalized = raw.replace('\\', "/");
        if normalized.starts_with('/') || has_drive_prefix(&normalized) {
            return Err(RelativePathError::Absolute {
                path: raw.to_owned(),
            });
        }
        let parts: Vec<&str> = normalized
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        if parts.is_empty() {
            return Err(RelativePathError::Empty);
        }
        if parts.contains(&"..") {
            return Err(RelativePathError::ParentTraversal {
                path: raw.to_owned(),
            });
        }
        if let Some(component) = parts.iter().find(|part| part.as_bytes().contains(&0u8)) {
            return Err(RelativePathError::InvalidComponent {
                path: raw.to_owned(),
                component: component.to_string(),
            });
        }
        Ok(Self(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_relative(&self) -> Result<RelativePath, RelativePathError> {
        RelativePath::new(&self.0)
    }
}

fn has_drive_prefix(value: &str) -> bool {
    let mut characters = value.chars();
    let drive = characters.next().is_some_and(|c| c.is_ascii_alphabetic())
        && characters.next() == Some(':');
    drive || value.starts_with("//")
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ProjectPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProjectPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for ProjectPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProjectPath".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A path relative to the project directory. `./` is accepted; \
                            absolute paths, drive letters, and `..` are refused.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_dot_slash_is_the_same_path() {
        assert_eq!(
            ProjectPath::new("./vendor/aurora.zupui")
                .expect("a project path")
                .as_str(),
            "vendor/aurora.zupui"
        );
        assert_eq!(
            ProjectPath::new(r".\vendor\aurora.zupui")
                .expect("a project path")
                .as_str(),
            "vendor/aurora.zupui"
        );
    }

    #[rstest::rstest]
    #[case::parent("../outside.zupui")]
    #[case::embedded_parent("vendor/../../outside.zupui")]
    #[case::rooted("/etc/passwd")]
    #[case::drive("C:/Windows/System32/x.dll")]
    #[case::unc(r"\\server\share\x.dll")]
    #[case::empty("   ")]
    fn a_path_outside_the_project_is_refused(#[case] value: &str) {
        assert!(
            ProjectPath::new(value).is_err(),
            "`{value}` names something the build must not read"
        );
    }

    #[test]
    fn a_project_path_becomes_portable_identity() {
        let path = ProjectPath::new("./branding/logo.svg").expect("a project path");
        assert_eq!(
            path.to_relative().expect("portable").as_str(),
            "branding/logo.svg"
        );
    }
}
