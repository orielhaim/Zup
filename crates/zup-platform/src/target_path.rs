use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use zup_core::{TargetOperatingSystem, TargetTriple};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetPathError {
    #[error("target path must not be empty")]
    Empty,

    #[error("target path `{path}` must be absolute for target `{target}`")]
    NotAbsolute { target: String, path: String },

    #[error("target path `{path}` contains `{component}`")]
    Traversal { path: String, component: String },

    #[error("target path `{path}` contains an unresolved template variable")]
    UnresolvedVariable { path: String },

    #[error("target path `{path}` contains a NUL character")]
    Nul { path: String },

    #[error("target path `{path}` contains an invalid component `{component}`")]
    InvalidComponent { path: String, component: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPath {
    target: TargetTriple,
    lexical: String,
}

impl TargetPath {
    pub fn new<T>(target: T, path: impl AsRef<str>) -> Result<Self, TargetPathError>
    where
        T: Borrow<TargetTriple>,
    {
        let target = target.borrow();
        let raw = path.as_ref();
        if raw.is_empty() {
            return Err(TargetPathError::Empty);
        }
        if raw.contains('\0') {
            return Err(TargetPathError::Nul {
                path: raw.to_owned(),
            });
        }
        if raw.contains("${") {
            return Err(TargetPathError::UnresolvedVariable {
                path: raw.to_owned(),
            });
        }

        let lexical = if target.operating_system() == TargetOperatingSystem::Windows {
            parse_windows_path(target, raw)?
        } else {
            parse_unix_path(target, raw)?
        };
        Ok(Self {
            target: target.clone(),
            lexical,
        })
    }

    pub fn target(&self) -> &TargetTriple {
        &self.target
    }

    pub fn as_str(&self) -> &str {
        &self.lexical
    }

    /// The separator this target spells its paths with.
    ///
    /// Windows lexical paths use `\`; every other target uses `/`, which is
    /// also a literal character inside a Windows segment.
    fn separator(&self) -> char {
        if self.target.operating_system() == TargetOperatingSystem::Windows {
            '\\'
        } else {
            '/'
        }
    }

    pub fn join(&self, suffix: impl AsRef<str>) -> Result<Self, TargetPathError> {
        let suffix = suffix.as_ref();
        if suffix.is_empty() {
            return Ok(self.clone());
        }
        if suffix.contains('\0') {
            return Err(TargetPathError::Nul {
                path: suffix.to_owned(),
            });
        }
        if suffix.contains("${") {
            return Err(TargetPathError::UnresolvedVariable {
                path: suffix.to_owned(),
            });
        }
        let windows_drive_suffix = self.target.operating_system() == TargetOperatingSystem::Windows
            && suffix.as_bytes().get(1) == Some(&b':')
            && suffix
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic());
        if suffix.starts_with('/')
            || (self.target.operating_system() == TargetOperatingSystem::Windows
                && suffix.starts_with('\\'))
            || windows_drive_suffix
        {
            return Err(TargetPathError::InvalidComponent {
                path: suffix.to_owned(),
                component: suffix.to_owned(),
            });
        }

        let separator = self.separator();
        let combined = if self.lexical.ends_with(separator) {
            format!("{}{}", self.lexical, suffix)
        } else {
            format!("{}{}{}", self.lexical, separator, suffix)
        };
        Self::new(&self.target, combined)
    }

    pub(crate) fn root_len(&self) -> usize {
        if self.target.operating_system() == TargetOperatingSystem::Windows {
            windows_root_len(&self.lexical)
        } else {
            1
        }
    }

    /// The containing directory, or `None` at the target's root.
    pub fn parent(&self) -> Option<Self> {
        let separator = self.separator();
        let root_len = self.root_len();
        if self.lexical.len() <= root_len {
            return None;
        }
        // Segments are located by separator search rather than by byte offsets:
        // a segment may end on a multi-byte character.
        let parent = match self.lexical.rfind(separator) {
            // The only segment left is the root itself.
            Some(index) if index + 1 == root_len => &self.lexical[..root_len],
            Some(index) => &self.lexical[..index],
            None => &self.lexical[..root_len],
        };
        Self::new(&self.target, parent).ok()
    }

    pub fn file_name(&self) -> Option<&str> {
        if self.lexical.len() <= self.root_len() {
            return None;
        }
        self.lexical.rsplit(self.separator()).next()
    }

    /// True when both paths name the same location under the target's own
    /// spelling rules.
    ///
    /// Windows paths are case-insensitive and use either separator, so
    /// `C:\Apps\Acme` and `c:/apps/acme/` are the same path. Unix targets
    /// compare byte-for-byte. Both sides are already normalized by
    /// [`TargetPath::new`], so only case sensitivity remains.
    pub fn equivalent(&self, other: &Self) -> bool {
        if self.target != other.target {
            return false;
        }
        if self.lexical == other.lexical {
            return true;
        }
        self.target.operating_system() == TargetOperatingSystem::Windows
            && self.lexical.eq_ignore_ascii_case(&other.lexical)
    }

    pub fn starts_with(&self, other: &Self) -> bool {
        if self.target != other.target {
            return false;
        }
        if self.lexical == other.lexical {
            return true;
        }
        let prefix_matches = if self.target.operating_system() == TargetOperatingSystem::Windows {
            self.lexical
                .get(..other.lexical.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&other.lexical))
        } else {
            self.lexical.starts_with(&other.lexical)
        };
        if !prefix_matches {
            return false;
        }
        let separator = self.separator();
        other.lexical.ends_with(separator)
            || self
                .lexical
                .as_bytes()
                .get(other.lexical.len())
                .is_some_and(|byte| *byte == separator as u8)
    }
}

fn parse_windows_path(target: &TargetTriple, raw: &str) -> Result<String, TargetPathError> {
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        let mut components = raw[2..].split(['\\', '/']);
        let server = components.next().unwrap_or_default();
        let share = components.next().unwrap_or_default();
        if server.is_empty()
            || share.is_empty()
            || matches!(server, "." | "..")
            || matches!(share, "." | "..")
            || server == "?"
        {
            return Err(TargetPathError::NotAbsolute {
                target: target.to_string(),
                path: raw.to_owned(),
            });
        }
        let mut path_components = vec![server, share];
        for component in components {
            if component.is_empty() {
                continue;
            }
            if component == "." || component == ".." {
                return Err(TargetPathError::Traversal {
                    path: raw.to_owned(),
                    component: component.to_owned(),
                });
            }
            path_components.push(component);
        }
        return Ok(format!(
            r"\\{}\{}",
            path_components[0],
            path_components[1..].join("\\")
        ));
    }

    let bytes = raw.as_bytes();
    let valid_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if !valid_drive {
        return Err(TargetPathError::NotAbsolute {
            target: target.to_string(),
            path: raw.to_owned(),
        });
    }

    let drive = raw[..1].to_owned();
    let mut components = Vec::new();
    for component in raw[3..].split(['\\', '/']) {
        if component.is_empty() {
            continue;
        }
        if component == "." || component == ".." {
            return Err(TargetPathError::Traversal {
                path: raw.to_owned(),
                component: component.to_owned(),
            });
        }
        components.push(component);
    }
    if components.is_empty() {
        Ok(format!("{drive}:\\"))
    } else {
        Ok(format!("{drive}:\\{}", components.join("\\")))
    }
}

fn windows_root_len(path: &str) -> usize {
    if !path.starts_with("\\\\") {
        return 3;
    }
    let mut separators = path[2..].match_indices('\\').map(|(index, _)| index + 2);
    match (separators.next(), separators.next()) {
        (Some(_), Some(second)) => second + 1,
        _ => path.len(),
    }
}

fn parse_unix_path(target: &TargetTriple, raw: &str) -> Result<String, TargetPathError> {
    if !raw.starts_with('/') {
        return Err(TargetPathError::NotAbsolute {
            target: target.to_string(),
            path: raw.to_owned(),
        });
    }
    let mut components = Vec::new();
    for component in raw[1..].split('/') {
        if component.is_empty() {
            continue;
        }
        if component == "." || component == ".." {
            return Err(TargetPathError::Traversal {
                path: raw.to_owned(),
                component: component.to_owned(),
            });
        }
        components.push(component);
    }
    if components.is_empty() {
        Ok("/".to_owned())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

impl fmt::Display for TargetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.lexical)
    }
}

impl Serialize for TargetPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("TargetPath", 2)?;
        state.serialize_field("target", &self.target)?;
        state.serialize_field("path", &self.lexical)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for TargetPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            target: TargetTriple,
            path: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.target, raw.path).map_err(serde::de::Error::custom)
    }
}
