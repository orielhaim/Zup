//! Portable planned resources and stable logical identity.

use serde::{Deserialize, Serialize};
use zup_core::{
    FileExtension, FileTypeId, NonEmptyString, Prerequisite, PrerequisiteArchitecture,
    PrerequisiteDetector, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage, Privilege,
    ProtocolScheme, RelativePath, ResourceKey, ServiceId, ServiceStart, Sha256Digest,
    ShortcutLocation, Template,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPrerequisite {
    pub id: PrerequisiteId,
    pub name: NonEmptyString,
    pub description: Option<String>,
    pub target: PrerequisiteArchitecture,
    pub detector: PrerequisiteDetector,
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
            detector: prerequisite.detector.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
            component: prerequisite.component.clone(),
            condition: prerequisite.when.clone(),
        }
    }
}

/// One active payload file in the desired installation.
///
/// Contains no build-machine paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFile {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub privilege: Privilege,
}

/// One active application shortcut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedShortcut {
    pub key: ResourceKey,
    pub location: ShortcutLocation,
    pub name: NonEmptyString,
    pub target: Template,
    pub arguments: Vec<String>,
    pub working_directory: Option<Template>,
    pub privilege: Privilege,
}

/// One active PATH entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPathEntry {
    pub key: ResourceKey,
    pub value: Template,
    pub privilege: Privilege,
}

/// One active service intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub name: NonEmptyString,
    pub display_name: Option<NonEmptyString>,
    pub binary: Template,
    pub arguments: Vec<String>,
    pub start: ServiceStart,
    /// Services always require machine privilege.
    pub privilege: Privilege,
}

/// One active URI-protocol registration intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedProtocol {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub executable: Template,
    pub args: Vec<String>,
    pub privilege: Privilege,
}

/// One active file-type registration intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFileType {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileTypeId,
    pub description: Option<String>,
    pub executable: Template,
    pub privilege: Privilege,
}
