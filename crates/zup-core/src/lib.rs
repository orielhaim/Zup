#![forbid(unsafe_code)]

mod build_plan;
mod condition;
mod digest;
mod ids;
mod installer;
mod model;
mod path;
mod prerequisite;
mod release;
mod target;
mod template;

pub use build_plan::{
    BuildPlan, CompiledIcon, IconRole, ResolvedAsset, ResolvedFile, ResolvedPlugin,
    ResolvedPrerequisite, TargetBuildPlan, TargetIcons,
};
pub use condition::{Condition, ConditionError};
pub use digest::{DigestParseError, Sha256Digest, hash_bytes, hash_reader};
pub use ids::{
    AppId, BackendResourceId, ComponentId, FileAssociationId, INSTALL_LOCATIONS, InstallLocation,
    NonEmptyString, PluginId, ProtocolScheme, ResourceKey, ServiceId, SessionId, TransactionId,
    ValueError, base64_decode, base64_encode,
};
pub use installer::{InstalledPreset, Installer, PresetAsset, PresetRuntime, UpdateConfig};
pub use model::{
    App, Component, ComponentGroup, ComponentProminence, FileAssociation, FileExtension,
    FileMapping, Frontend, Install, InstallDirectory, InstallScope, Launcher, LauncherLocation,
    MAX_PRESET_SETTINGS_BYTES, PathEntry, PluginBinding, Privilege, Protocol, SelectionRequirement,
    Service, ServiceStart, Source, Ui, uri_placeholder_count,
};
pub use path::{ProjectPath, RelativePath, RelativePathError};
pub use prerequisite::{
    FileVersion, InstalledPackage, InstalledPackageId, MAX_INSTALLED_PACKAGE_ID_BYTES,
    MAX_PREREQUISITE_ARGUMENT_BYTES, MAX_PREREQUISITE_ARGUMENTS, MAX_PREREQUISITE_ID_BYTES,
    MAX_PREREQUISITE_PACKAGE_BYTES, MAX_RUNTIME_REQUIREMENT_ID_BYTES, Prerequisite,
    PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    PrerequisiteRequirement, Runtime, RuntimeRequirementId,
};
pub use release::{IdentityError, MAX_IDENTITY_COMPONENTS, ReleaseIdentity};
pub use target::{
    ResolvedTargetConfig, TargetArchitecture, TargetOperatingSystem, TargetOverrides,
    TargetParseError, TargetProfile, TargetProfileId, TargetTriple, host_architecture,
    host_operating_system,
};
pub use template::{Template, TemplateError, TemplatePart, Variable, VariableValue};

pub const PLUGIN_PAYLOAD_ROOT: &str = "__zup_plugins__";
pub const MAX_PLUGIN_ARTIFACTS: usize = 128;

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SelectedScope {
    User,
    Machine,
}

impl SelectedScope {
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
