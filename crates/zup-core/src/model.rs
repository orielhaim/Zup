//! Normalized, platform-independent installer model.

use std::path::PathBuf;

use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::condition::Condition;
use crate::ids::{
    AppId, ComponentId, FileTypeId, NonEmptyString, PluginId, ProtocolScheme, ServiceId,
};
use crate::template::Template;
use crate::value::ValueError;

/// Application identity and display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: AppId,
    pub name: NonEmptyString,
    #[schemars(with = "String")]
    pub version: Version,
    #[serde(default)]
    pub publisher: Option<NonEmptyString>,
    #[serde(default)]
    pub main: Option<Template>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiBranding {
    #[serde(default, alias = "icon")]
    pub logo: Option<Template>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub theme: UiTheme,
    #[serde(default, alias = "license_url")]
    pub license_link: Option<String>,
    #[serde(default, alias = "legal")]
    pub legal_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    System,
    Light,
    Dark,
}

/// Installation scope and destination templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub scope: InstallScope,
    #[serde(default)]
    pub directory: InstallDirectory,
    #[serde(
        default,
        alias = "allow_install_directory",
        alias = "allow_install_dir"
    )]
    pub allow_directory_override: bool,
}

/// Scope an installer may target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    User,
    Machine,
    Either,
}

impl InstallScope {
    /// True when a per-user install is a legal outcome of this scope.
    pub const fn allows_user(self) -> bool {
        matches!(self, Self::User | Self::Either)
    }

    /// True when a per-machine install is a legal outcome of this scope.
    pub const fn allows_machine(self) -> bool {
        matches!(self, Self::Machine | Self::Either)
    }
}

impl std::fmt::Display for InstallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Machine => "machine",
            Self::Either => "either",
        })
    }
}

/// Unresolved install-root templates by scope.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallDirectory {
    #[serde(default)]
    pub user: Option<Template>,
    #[serde(default)]
    pub machine: Option<Template>,
}

/// A selectable application component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub id: ComponentId,
    pub name: NonEmptyString,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_true")]
    pub default: bool,
    #[serde(default)]
    pub requires: Vec<ComponentId>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    pub id: PluginId,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// Declarative file mapping. Patterns are not expanded here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileMapping {
    pub source: String,
    pub destination: Template,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
    /// Allow a pattern that matches zero files. Default: reject.
    #[serde(default)]
    pub allow_empty: bool,
}

/// Portable application-shortcut location.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutLocation {
    StartMenu,
    Desktop,
}

impl std::fmt::Display for ShortcutLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StartMenu => "start-menu",
            Self::Desktop => "desktop",
        })
    }
}

/// High-level application launcher intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shortcut {
    pub location: ShortcutLocation,
    pub name: NonEmptyString,
    pub target: Template,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub working_directory: Option<Template>,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// A logical PATH entry to add to the installation-appropriate scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathEntry {
    pub value: Template,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// Platform-neutral service start policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStart {
    Automatic,
    Manual,
    Disabled,
}

/// Platform-neutral service intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub id: ServiceId,
    pub name: NonEmptyString,
    #[serde(default)]
    pub display_name: Option<NonEmptyString>,
    pub binary: Template,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub start: ServiceStart,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// URI-scheme launch capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    pub scheme: ProtocolScheme,
    pub executable: Template,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// File extension registration intent. Does not set default handlers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileType {
    pub extension: FileExtension,
    pub id: FileTypeId,
    #[serde(default)]
    pub description: Option<String>,
    pub executable: Template,
    #[serde(default)]
    pub when: Option<Condition>,
}

/// A bare file extension such as `.acme`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct FileExtension(String);

impl FileExtension {
    /// Create an extension. Requires a leading `.` and no path separators.
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let extension = value.as_ref().trim();
        let valid = extension.starts_with('.')
            && extension.len() > 1
            && !extension.contains('/')
            && !extension.contains('\\')
            && !extension.contains('\0');
        if !valid {
            return Err(ValueError::InvalidExtension {
                extension: extension.to_owned(),
            });
        }
        Ok(Self(extension.to_owned()))
    }

    /// Borrow the extension as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FileExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for FileExtension {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FileExtension {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    User,
    Machine,
}

/// Payload source for the installer (build-time, not install-time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Source {
    pub directory: PathBuf,
}

impl Source {
    /// Create a source root, rejecting an empty path.
    pub fn new(directory: PathBuf) -> Result<Self, ValueError> {
        if directory.as_os_str().is_empty() {
            return Err(ValueError::Empty {
                kind: "source.directory",
            });
        }
        Ok(Self { directory })
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            directory: PathBuf,
        }

        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.directory).map_err(serde::de::Error::custom)
    }
}
