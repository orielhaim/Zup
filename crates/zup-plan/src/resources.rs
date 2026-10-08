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
    pub description: Option<String>,
    pub target: PrerequisiteArchitecture,
    pub requirement: PrerequisiteRequirement,
    pub package: PrerequisitePackage,
    pub installer: PrerequisiteInstaller,
    pub component: Option<zup_core::ComponentId>,
    pub condition: Option<zup_core::Condition>,
}

impl PlannedPrerequisite {
    pub fn from_prerequisite(prerequisite: &Prerequisite) -> Self {
        Self {
            id: prerequisite.id.clone(),
            name: prerequisite.name.clone(),
            description: prerequisite.description.clone(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
            component: prerequisite.component.clone(),
            condition: prerequisite.when.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFile {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub privilege: Privilege,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedLauncher {
    pub key: ResourceKey,
    pub location: LauncherLocation,
    pub name: NonEmptyString,
    pub target: Template,
    pub arguments: Vec<String>,
    pub working_directory: Option<Template>,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPathEntry {
    pub key: ResourceKey,
    pub value: Template,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub name: NonEmptyString,
    pub display_name: Option<NonEmptyString>,
    pub binary: Template,
    pub arguments: Vec<String>,
    pub start: ServiceStart,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedProtocol {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub executable: Template,
    pub args: Vec<String>,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFileAssociation {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileAssociationId,
    pub description: Option<String>,
    pub executable: Template,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}
