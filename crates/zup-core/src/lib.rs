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
mod encoding;
mod ids;
mod installer;
mod location;
mod model;
mod path;
mod prerequisite;
mod release;
mod resource_key;
mod target;
mod template;
mod value;

pub use condition::{Condition, ConditionError};
pub use digest::{DigestParseError, Sha256Digest, hash_bytes, hash_reader};
pub use encoding::{base64_decode, base64_encode};
pub use ids::{
    AppId, BackendResourceId, ComponentId, FileAssociationId, NonEmptyString, PluginId,
    ProtocolScheme, ServiceId,
};
pub use installer::{Installer, UpdateConfig};
pub use location::{INSTALL_LOCATIONS, InstallLocation};
pub use model::{
    App, Component, FileAssociation, FileExtension, FileMapping, Frontend, Install,
    InstallDirectory, InstallScope, Launcher, LauncherLocation, PathEntry, PluginBinding,
    Privilege, Protocol, Service, ServiceStart, Source, UiBranding, UiTheme,
};
pub use path::{RelativePath, RelativePathError};
pub use prerequisite::{
    FileVersion, InstalledPackage, InstalledPackageId, MAX_INSTALLED_PACKAGE_ID_BYTES,
    MAX_PREREQUISITE_ARGUMENT_BYTES, MAX_PREREQUISITE_ARGUMENTS, MAX_PREREQUISITE_ID_BYTES,
    MAX_PREREQUISITE_PACKAGE_BYTES, MAX_RUNTIME_REQUIREMENT_ID_BYTES, Prerequisite,
    PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    PrerequisiteRequirement, Runtime, RuntimeRequirementId,
};
pub use release::{IdentityError, MAX_IDENTITY_COMPONENTS, ReleaseIdentity};
pub use resource_key::ResourceKey;
pub use target::{
    ResolvedTargetConfig, TargetArchitecture, TargetOperatingSystem, TargetOverrides,
    TargetParseError, TargetProfile, TargetProfileId, TargetTriple, host_architecture,
    host_operating_system,
};
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
    /// Default authorization for resources that a scope places on the host.
    ///
    /// A scope says *where* an application lives, not *how* the host authorizes
    /// work. Planning uses this as the default for resources that declare no
    /// narrower requirement; every resource may still carry its own
    /// [`Privilege`], and nothing outside authoring derives authorization from
    /// a scope.
    pub const fn authorization(self) -> Privilege {
        match self {
            Self::User => Privilege::User,
            Self::Machine => Privilege::System,
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
