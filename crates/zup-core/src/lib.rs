//! Platform-independent installer engine primitives for zup.
//!
//! This crate owns the normalized Installer IR and the core domain types
//! used to describe installation intent: identifiers, templates, conditions,
//! components, and resource collections.
//!
//! It has no knowledge of `zup.toml` or any authoring format.

#![forbid(unsafe_code)]

mod condition;
mod digest;
mod ids;
mod installer;
mod model;
mod path;
mod resource_key;
mod template;
mod value;

pub use condition::{Condition, ConditionError};
pub use digest::{DigestParseError, Sha256Digest, hash_reader};
pub use ids::{
    AppId, ComponentId, FileTypeId, NonEmptyString, PluginId, ProtocolScheme, ServiceId,
};
pub use installer::{Installer, UpdateConfig};
pub use model::{
    App, Component, FileExtension, FileMapping, FileType, Install, InstallDirectory, InstallScope,
    PathEntry, PluginBinding, Privilege, Protocol, Service, ServiceStart, Shortcut,
    ShortcutLocation, Source,
};
pub use path::{RelativePath, RelativePathError};
pub use resource_key::ResourceKey;
pub use template::{Template, TemplateError, TemplatePart, Variable, VariableValue};
pub use value::ValueError;

pub const PLUGIN_PAYLOAD_ROOT: &str = "__zup_plugins__";
pub const MAX_PLUGIN_ARTIFACTS: usize = 128;

/// Concrete installation scope chosen for one plan.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SelectedScope {
    User,
    Machine,
}

impl SelectedScope {
    /// Privilege implied by the installation scope.
    pub const fn privilege(self) -> Privilege {
        match self {
            Self::User => Privilege::User,
            Self::Machine => Privilege::Machine,
        }
    }
}

impl std::fmt::Display for SelectedScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Machine => "machine",
        })
    }
}
