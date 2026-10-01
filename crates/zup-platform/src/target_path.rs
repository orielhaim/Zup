//! A path bound to the target it names, not to the machine doing the work.
//!
//! Lexical mechanics — roots, components, separators, joining, parents — come
//! from `typed-path`, so a Windows path is parsed as Windows even when the build
//! host is not. What lives here is installer policy: a target path is absolute
//! for its own target, carries no unresolved template variable, and refuses to
//! climb out of the directory it names.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use typed_path::{
    PathType, Utf8TypedComponent, Utf8TypedComponents, Utf8TypedPath, Utf8TypedPathBuf,
    Utf8UnixComponent, Utf8WindowsComponent, Utf8WindowsPath, Utf8WindowsPrefix,
};
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

    /// Windows device and verbatim namespaces name kernel objects rather than
    /// files, so a path in one is not a place an installer can write.
    #[error("Windows device path `{path}` is not supported")]
    DevicePath { path: String },

    #[error("target path `{path}` contains an invalid component `{component}`")]
    InvalidComponent { path: String, component: String },
}

/// An absolute, fully resolved path on one target.
///
/// Two paths are the same location when they are [`equivalent`], which follows
/// the target's own spelling: a Windows target compares case-insensitively and
/// accepts either separator, and any other target compares byte for byte.
///
/// [`equivalent`]: TargetPath::equivalent
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPath {
    target: TargetTriple,
    /// Canonical spelling: the target's own separators, no redundant or
    /// relative components, no trailing separator unless the path is a root.
    lexical: Utf8TypedPathBuf,
}

impl TargetPath {
    /// Bind `path` to `target`, or refuse it.
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
        let lexical = canonical(target, raw)?;
        Ok(Self {
            target: target.clone(),
            lexical,
        })
    }

    pub fn target(&self) -> &TargetTriple {
        &self.target
    }

    /// The canonical spelling of this path.
    pub fn as_str(&self) -> &str {
        self.lexical.as_str()
    }

    /// This path as Windows spells it, or `None` when the target is not Windows.
    ///
    /// The spelling is already canonical, so this is a view rather than a
    /// re-parse of the caller's text.
    pub fn as_windows(&self) -> Option<&Utf8WindowsPath> {
        match self.lexical.to_path() {
            Utf8TypedPath::Windows(path) => Some(path),
            Utf8TypedPath::Unix(_) => None,
        }
    }

    /// Extend this path with a relative `suffix`.
    ///
    /// The suffix may not name a root or a prefix of its own, may not climb
    /// out with `..`, and may not carry an unresolved template variable:
    /// a target path names one location, so anything that would move it is
    /// refused rather than resolved.
    pub fn join(&self, suffix: impl AsRef<str>) -> Result<Self, TargetPathError> {
        let suffix = suffix.as_ref();
        // A suffix names a location below this one, so a root, a prefix or a
        // `..` would relocate the path rather than extend it. `push_checked`
        // already refuses the first two, but it resolves a `..` that stays
        // inside the path, which is a hop this path must not contain.
        for component in lexical(&self.target, suffix).components() {
            if component.is_parent() {
                return Err(TargetPathError::Traversal {
                    path: suffix.to_owned(),
                    component: component.as_str().to_owned(),
                });
            }
        }
        let mut lexical = self.lexical.clone();
        lexical
            .push_checked(suffix)
            .map_err(|_| TargetPathError::InvalidComponent {
                path: suffix.to_owned(),
                component: suffix.to_owned(),
            })?;
        Self::new(&self.target, lexical.as_str())
    }

    /// The containing directory, or `None` at the target's root.
    pub fn parent(&self) -> Option<Self> {
        self.lexical
            .parent()
            .and_then(|parent| Self::new(&self.target, parent.as_str()).ok())
    }

    /// The final component, or `None` when the path is a root.
    pub fn file_name(&self) -> Option<&str> {
        self.lexical.file_name()
    }

    /// True when both paths name the same location under the target's own
    /// spelling rules.
    pub fn equivalent(&self, other: &Self) -> bool {
        self.target == other.target && components_match(&self.lexical, &other.lexical, &self.target)
    }

    /// True when this path is `other` or sits below it.
    pub fn starts_with(&self, other: &Self) -> bool {
        if self.target != other.target {
            return false;
        }
        let folds_case = folds_case(&self.target);
        let mut mine = self.lexical.to_path().components();
        for theirs in other.lexical.to_path().components() {
            match mine.next() {
                Some(mine) if same_component(mine.as_str(), theirs.as_str(), folds_case) => {}
                _ => return false,
            }
        }
        true
    }

    /// Extend this path with the location `child` names below its own root.
    ///
    /// A resolved install location carries its own root, which this path
    /// already has, so only the names below it are joined on.
    pub(crate) fn extend_with(&self, child: &TargetPath) -> Result<Self, TargetPathError> {
        self.join_segments(child.segments())
    }

    /// Extend this path with a run of relative segments.
    pub(crate) fn join_segments<S: AsRef<str>>(
        &self,
        segments: impl IntoIterator<Item = S>,
    ) -> Result<Self, TargetPathError> {
        segments
            .into_iter()
            .try_fold(self.clone(), |path, segment| path.join(segment))
    }

    /// The ordinary components of this path, without the root they hang from.
    fn segments(&self) -> impl Iterator<Item = &str> {
        self.lexical
            .to_path()
            .components()
            .filter_map(|component| match component {
                Utf8TypedComponent::Unix(Utf8UnixComponent::Normal(name))
                | Utf8TypedComponent::Windows(Utf8WindowsComponent::Normal(name)) => Some(name),
                _ => None,
            })
    }
}

/// The ordinary components of `text` read as a path for `target`, or the
/// component that stops it from being a suffix.
///
/// A template literal is a suffix of a path already being built: it may begin
/// at the target's root, but a prefix or a `..` of its own would relocate the
/// path instead of extending it.
pub(crate) fn suffix_components<'a>(
    target: &TargetTriple,
    text: &'a str,
) -> Result<Vec<&'a str>, &'a str> {
    let mut segments = Vec::new();
    for component in lexical(target, text).components() {
        match component {
            Utf8TypedComponent::Windows(Utf8WindowsComponent::Prefix(prefix)) => {
                return Err(prefix.as_str());
            }
            component if component.is_normal() => segments.push(component.as_str()),
            component if component.is_root() => {}
            component => return Err(component.as_str()),
        }
    }
    Ok(segments)
}

/// Whether `raw` names a location the target can install to, in its canonical
/// spelling.
fn canonical(target: &TargetTriple, raw: &str) -> Result<Utf8TypedPathBuf, TargetPathError> {
    let path = lexical(target, raw);

    if let Utf8TypedPath::Windows(windows) = path
        && let Some(prefix) = windows.components().prefix_kind()
        && !is_installable_prefix(prefix)
    {
        return Err(TargetPathError::DevicePath {
            path: raw.to_owned(),
        });
    }

    if !is_absolute(&path) {
        return Err(TargetPathError::NotAbsolute {
            target: target.to_string(),
            path: raw.to_owned(),
        });
    }

    let components = path.components();
    for component in components.clone() {
        if component.is_parent() {
            return Err(TargetPathError::Traversal {
                path: raw.to_owned(),
                component: component.as_str().to_owned(),
            });
        }
    }

    Ok(assemble(target, components))
}

/// Whether a Windows prefix roots a path at somewhere an installer can write.
///
/// A drive and a network share do. `\\?\`, `\\.\` and `\\??\` reach the kernel
/// through the same UNC spelling, and a `.` or `..` server or share names no
/// machine on the network at all.
fn is_installable_prefix(prefix: Utf8WindowsPrefix<'_>) -> bool {
    match prefix {
        Utf8WindowsPrefix::Disk(_) => true,
        Utf8WindowsPrefix::UNC(server, share) => {
            !matches!(server, "." | ".." | "?" | "??") && !matches!(share, "." | "..")
        }
        _ => false,
    }
}

/// Spell `components` the way the target writes a path.
///
/// Every component is already known to be a prefix, a root, or a name, so this
/// only chooses separators: the target's own between names, no trailing
/// separator unless the target's root requires one, and the prefix verbatim so
/// an authored drive letter keeps its case.
fn assemble(target: &TargetTriple, components: Utf8TypedComponents<'_>) -> Utf8TypedPathBuf {
    let separator = separator(target);
    let mut text = String::new();
    let mut rooted = false;
    for component in components {
        match component {
            Utf8TypedComponent::Windows(Utf8WindowsComponent::Prefix(prefix)) => {
                // A prefix carries its own separators, so a UNC prefix spelled
                // with `/` still comes out spelled with `\`.
                text.push_str(&prefix.as_str().replace('/', separator));
                // A UNC prefix is already a root, so `\\server\share` is written
                // without a trailing separator. A drive is not, so `C:` needs one.
                rooted = !matches!(prefix.kind(), Utf8WindowsPrefix::Disk(_));
                if !rooted {
                    text.push_str(separator);
                    rooted = true;
                }
            }
            // On Windows the root separator was written with the drive; on Unix
            // the root is all there is to write.
            _ if rooted && component.is_root() => {}
            component if component.is_root() => {
                text.push_str(separator);
                rooted = true;
            }
            component => {
                if !text.is_empty() && !text.ends_with(separator) {
                    text.push_str(separator);
                }
                text.push_str(component.as_str());
            }
        }
    }

    lexical(target, &text).to_path_buf()
}

/// The separator this target spells its paths with.
fn separator(target: &TargetTriple) -> &'static str {
    if target.operating_system() == TargetOperatingSystem::Windows {
        typed_path::constants::windows::SEPARATOR_STR
    } else {
        typed_path::constants::unix::SEPARATOR_STR
    }
}

fn lexical<'a>(target: &TargetTriple, raw: &'a str) -> Utf8TypedPath<'a> {
    Utf8TypedPath::new(
        raw,
        match target.operating_system() {
            TargetOperatingSystem::Windows => PathType::Windows,
            _ => PathType::Unix,
        },
    )
}

/// Whether `path` is absolute on its own target.
///
/// A Windows path is rooted at a drive or a UNC share; `\Acme` is not absolute
/// because it names a location on whichever drive happens to be current.
fn is_absolute(path: &Utf8TypedPath<'_>) -> bool {
    match path {
        Utf8TypedPath::Unix(path) => path.has_root(),
        Utf8TypedPath::Windows(path) => path.has_root() && path.components().has_prefix(),
    }
}

/// Windows spells paths case-insensitively; every other target compares bytes.
fn folds_case(target: &TargetTriple) -> bool {
    target.operating_system() == TargetOperatingSystem::Windows
}

fn same_component(left: &str, right: &str, folds_case: bool) -> bool {
    if folds_case {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn components_match(
    left: &Utf8TypedPathBuf,
    right: &Utf8TypedPathBuf,
    target: &TargetTriple,
) -> bool {
    let folds_case = folds_case(target);
    let mut left = left.to_path().components();
    let mut right = right.to_path().components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right))
                if same_component(left.as_str(), right.as_str(), folds_case) => {}
            _ => return false,
        }
    }
}

impl fmt::Display for TargetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
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
        state.serialize_field("path", self.as_str())?;
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
