#![forbid(unsafe_code)]

mod recovery;
mod storage;
pub use recovery::{
    ActiveLauncher, ActiveLaunchers, DeploymentSettlement, RecoveryCleanup, RecoveryKind,
    RecoverySummary,
};
pub use storage::DeploymentStorageEstimate;

mod build_plan;
mod command;
mod condition;
pub use command::CommandSpec;
mod deployment_id;
mod digest;
pub use deployment_id::{
    ActivationRevision, DeploymentIdentityError, InstallationId, MaterializationId,
};
mod generation;
pub use generation::{
    DEPLOYMENTS_DIRECTORY, DeploymentContractId, GenerationDescriptor, GenerationError,
    GenerationFile, MAINTENANCE_DEPLOYMENTS_DIRECTORY, MaterializationRef, MaterializationSpec,
    MaterializationTree, PayloadTreeId,
};
mod ids;
mod installer;
mod model;
mod service;
mod service_argument;
pub use service::{
    ChangedServiceState, InitialServiceState, NativeServiceRegistration, NativeServiceTimeouts,
    ObservedServiceRegistration, ServiceExecution, ServiceFailurePolicy, ServiceLifecycle,
    ServiceOwnershipClaim, ServiceReadiness, ServiceTimeouts, SystemdRestartCondition,
    WindowsServiceAccount,
};
pub use service_argument::{InvalidServiceArgument, ServiceArgument, service_job_name};
mod package;
pub use package::{PackagePlan, PayloadEntry, PluginArtifact, PrerequisiteArtifact};
mod path;
mod payload;
pub use payload::{
    FixedReplacement, PayloadId, PayloadMapping, PayloadPlacement, PayloadRequirement, ResourceId,
    ResourceIdError,
};
mod prerequisite;
mod release;
mod target;
mod target_path;
pub use target_path::{TargetPath, TargetPathError};
mod template;

pub use build_plan::{
    BuildPlan, CompiledIcon, IconRole, ResolvedAsset, ResolvedFile, ResolvedPlugin,
    ResolvedPrerequisite, TargetBuildPlan, TargetIcons,
};
pub use condition::{Condition, ConditionError};
pub use digest::{DigestParseError, Sha256Digest, hash_bytes, hash_reader};
pub use ids::{
    AppId, BackendResourceId, ComponentId, DeclaredResourceId, FileAssociationId, FileResourceId,
    FixedFileInstance, INSTALL_LOCATIONS, InstallLocation, MaintenanceArtifact, NativeFileId,
    NativeObjectIdentity, NonEmptyString, PluginId, ProtocolScheme, ResourceKey, ServiceId,
    SessionId, TransactionId, ValueError, base64_decode, base64_encode,
};
pub use installer::{
    InstalledPreset, Installer, MAX_PRESET_BINARY_BYTES, PresetAsset, PresetRuntime, UpdateConfig,
};
pub use model::{
    App, Component, ComponentGroup, ComponentProminence, FileAssociation, FileExtension,
    FileMapping, Frontend, Install, InstallDirectory, InstallScope, Launcher, LauncherLocation,
    MAX_PRESET_SETTINGS_BYTES, PathEntry, PluginBinding, Privilege, Protocol, ScopeSelectionError,
    SelectionRequirement, Service, ServiceStart, Source, Ui, uri_placeholder_count,
};
pub use path::{ProjectPath, RelativePath, RelativePathError};
pub use prerequisite::{
    FileVersion, InstalledPackage, InstalledPackageId, MAX_INSTALLED_PACKAGE_ID_BYTES,
    MAX_PREREQUISITE_ARGUMENT_BYTES, MAX_PREREQUISITE_ARGUMENTS, MAX_PREREQUISITE_ID_BYTES,
    MAX_PREREQUISITE_PACKAGE_BYTES, MAX_RUNTIME_REQUIREMENT_ID_BYTES, Prerequisite,
    PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    PrerequisiteRequirement, Runtime, RuntimeRequirementId,
};
pub use release::{
    IdentityError, MAX_IDENTITY_COMPONENTS, ReleaseCandidate, ReleaseIdentity, ReleasePin,
};
pub use target::{
    ResolvedTargetConfig, Target, TargetArchitecture, TargetOperatingSystem, TargetOverrides,
    TargetParseError, TargetProfile, TargetProfileId, host_architecture, host_operating_system,
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

mod document;
mod source_policy;
pub use document::{DocumentError, publish, publish_new};
pub use source_policy::{PortableSourceFilePolicy, SourceFilePolicy};

mod phase;
pub use phase::OperationPhase;

mod persistent;
mod retention;
pub use persistent::{
    DataCompatibility, InvalidServiceDataWriters, PersistentAccess, PersistentDirectory,
    PersistentLocation, ServiceDataWriters,
};
pub use retention::GenerationReaders;

mod service_graph;
pub use service_graph::{ServiceGraph, service_dependency_keys, validate_service_graph};
