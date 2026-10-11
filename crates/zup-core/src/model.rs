use std::collections::BTreeMap as Map;
use std::path::PathBuf;

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::condition::Condition;
use crate::ids::ValueError;
use crate::ids::{
    AppId, ComponentId, FileAssociationId, NonEmptyString, PluginId, ProtocolScheme, ServiceId,
};
use crate::path::ProjectPath;
use crate::template::Template;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: AppId,
    pub name: NonEmptyString,
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub version: Version,
    #[serde(default)]
    pub publisher: Option<NonEmptyString>,
    #[serde(default)]
    pub main: Option<Template>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
/// The preset this application presents, as its author configures it.
///
/// The only customization system an application has. What a preset draws is the
/// preset's business, so there is nothing here for an application author to
/// describe a window: the package is chosen, and its own settings are filled in.
pub struct Ui {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The `.zupui` to present. Absent means the preset Zup ships.
    pub preset: Option<ProjectPath>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    /// Values for the chosen preset's settings, validated against the schema the
    /// package carries before anything is composed.
    pub settings: Map<String, serde_json::Value>,
}

pub const MAX_PRESET_SETTINGS_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Frontend {
    #[default]
    Gui,
    Console,
    Headless,
}

impl Frontend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Console => "console",
            Self::Headless => "headless",
        }
    }

    pub const fn is_headless(self) -> bool {
        matches!(self, Self::Headless)
    }
}

impl std::fmt::Display for Frontend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
/// Installation scope and destination templates.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
/// Scope an installer may target.
pub enum InstallScope {
    User,
    Machine,
    Either,
}

impl InstallScope {
    pub const fn allows_user(self) -> bool {
        matches!(self, Self::User | Self::Either)
    }

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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
/// Unresolved install-root templates by scope.
pub struct InstallDirectory {
    #[serde(default)]
    pub user: Option<Template>,
    #[serde(default)]
    pub machine: Option<Template>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
/// How clearly a component group should be presented.
///
/// This is about the group as one decision, not about where a preset draws it.
/// `Auto` is conservative: a group with nothing optional to choose disappears,
/// a group that requires an explicit selection is primary, and everything else
/// is secondary.
pub enum ComponentProminence {
    #[default]
    Auto,
    /// The person should see that this decision exists before installing.
    Primary,
    /// A sensible default. The whole group can stay out of the happy path.
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
/// Whether a group's defaults are enough to install.
pub enum SelectionRequirement {
    #[default]
    /// The declared defaults are a valid choice.
    Defaulted,
    /// At least one optional component in the group must be selected.
    Explicit,
}

/// belongs to the implicit default group, so a package that never mentions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ComponentGroup {
    pub id: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<NonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub prominence: ComponentProminence,
    #[serde(default)]
    pub selection: SelectionRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The group this component belongs to. Absent means the implicit group.
    pub group: Option<NonEmptyString>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    pub id: PluginId,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FileMapping {
    pub source: String,
    pub destination: Template,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
    #[serde(default)]
    /// Allow a pattern that matches zero files. Default: reject.
    pub allow_empty: bool,
    #[serde(default)]
    /// This file is intended to be executable.
    ///
    /// The intent is portable; how it is honoured is not. A backend that has
    /// filesystem modes applies one, and a backend that does not has nothing to
    /// change. What is *not* portable is a raw mode, so there is deliberately no
    /// way to write `0755` here: a build machine on Windows has no meaningful mode
    /// bits to preserve, and a mode authored on Linux would silently disagree with
    /// the same manifest built elsewhere.
    ///
    /// Not inferred from the file's bytes either. A script, a data file and an ELF
    /// are each identifiable without permission bits, but a file happening to be
    /// ELF does not make it something a user should be able to run.
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "kebab-case")]
/// Portable application launcher location.
pub enum LauncherLocation {
    Menu,
    Desktop,
}

impl std::fmt::Display for LauncherLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Menu => "menu",
            Self::Desktop => "desktop",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Launcher {
    pub location: LauncherLocation,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PathEntry {
    pub value: Template,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
/// Platform-neutral service start policy.
pub enum ServiceStart {
    Automatic,
    Manual,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    pub scheme: ProtocolScheme,
    pub executable: Template,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub when: Option<Condition>,
}

impl Protocol {
    pub fn uri_placeholder_count(&self) -> usize {
        uri_placeholder_count(&self.args)
    }
}

pub fn uri_placeholder_count(args: &[String]) -> usize {
    args.iter().filter(|arg| arg.as_str() == "%1").count()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FileAssociation {
    pub extension: FileExtension,
    pub id: FileAssociationId,
    #[serde(default)]
    pub description: Option<String>,
    pub executable: Template,
    #[serde(default)]
    pub when: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct FileExtension(String);

impl FileExtension {
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

/// Authorization an operation needs on the target host.
///
/// This names *who* must perform an operation, never *how* the host obtains
/// that authority. Elevation, impersonation, and policy prompts are platform
/// concerns resolved by a platform runtime, not by this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    /// The signed-in user is enough.
    User,
    /// Host-wide authority is required.
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
/// Payload source for the installer (build-time, not install-time).
pub struct Source {
    pub directory: PathBuf,
}

impl Source {
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
