//! A path a person wrote, relative to the project.
//!
//! [`ProjectPath`] and [`RelativePath`] are different types on purpose. The
//! second is portable identity: the normalized form that ends up inside a
//! package, which has no `.` component because a normalized path does not. The
//! first is what someone types, where `./` is how they say "in this project" and
//! refusing it would make the obvious way of writing a path an error.
//!
//! The refusals that matter are here rather than left to whoever opens the file:
//! a path that leaves the project or names a machine is a mistake where it was
//! written, and one only caught later is one that reaches a build.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::path::{RelativePath, RelativePathError};

/// A project-relative path as it was written in a configuration file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectPath(String);

impl ProjectPath {
    /// Read a project-relative path, accepting a leading `./`.
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

    /// The path as written, with any `./` removed.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The portable identity form, for a value that goes into a package.
    pub fn to_relative(&self) -> Result<RelativePath, RelativePathError> {
        RelativePath::new(&self.0)
    }
}

/// A `C:` or `C:/` prefix, or a `//host` share: a machine, not a project.
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

    /// `./` is how a person says "in this project", so the obvious spelling has
    /// to work.
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

    /// Every way of naming something outside the project is refused where it is
    /// written, because a build that discovered it later would have done real
    /// work first.
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

    /// The portable form is what a package carries, and it is reachable without
    /// the caller re-parsing the string.
    #[test]
    fn a_project_path_becomes_portable_identity() {
        let path = ProjectPath::new("./branding/logo.svg").expect("a project path");
        assert_eq!(
            path.to_relative().expect("portable").as_str(),
            "branding/logo.svg"
        );
    }
}
