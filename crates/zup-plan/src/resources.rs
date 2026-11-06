use serde::{Deserialize, Serialize};
use zup_core::{
    FileAssociationId, FileExtension, LauncherLocation, NonEmptyString, Prerequisite,
    PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    PrerequisiteRequirement, Privilege, ProtocolScheme, RelativePath, ResourceKey, SelectedScope,
    ServiceId, ServiceStart, Sha256Digest, Template,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPrerequisite {
    pub id: PrerequisiteId,
    pub name: NonEmptyString,
    pub target: PrerequisiteArchitecture,
    pub requirement: PrerequisiteRequirement,
    pub package: PrerequisitePackage,
    pub installer: PrerequisiteInstaller,
}

impl PlannedPrerequisite {
    pub fn from_prerequisite(prerequisite: &Prerequisite) -> Self {
        Self {
            id: prerequisite.id.clone(),
            name: prerequisite.name.clone(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredFile<P = Template> {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: P,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub privilege: Privilege,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredLauncher<P = Template, L = ()> {
    pub key: ResourceKey,
    pub location: LauncherLocation,
    pub launcher_path: L,
    pub name: NonEmptyString,
    pub target: P,
    pub arguments: Vec<String>,
    pub working_directory: Option<P>,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredPathEntry<P = Template> {
    pub key: ResourceKey,
    pub value: P,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredService<P = Template> {
    pub failure: zup_core::ServiceFailurePolicy,
    pub execution: zup_core::ServiceExecution,
    pub key: ResourceKey,
    pub id: ServiceId,
    pub name: NonEmptyString,
    pub display_name: Option<NonEmptyString>,
    pub command: crate::CommandSpec<P>,
    pub start: ServiceStart,
    pub lifecycle: zup_core::ServiceLifecycle,
    pub readiness: zup_core::ServiceReadiness,
    pub requires_files: Vec<P>,
    pub requires_services: Vec<zup_core::ServiceId>,
    pub dependency_names: Vec<String>,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredProtocol<P = Template> {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub command: crate::CommandSpec<P>,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredFileAssociation<P = Template> {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileAssociationId,
    pub description: Option<String>,
    pub command: crate::CommandSpec<P>,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}
