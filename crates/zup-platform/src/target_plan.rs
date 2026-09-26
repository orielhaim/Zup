//! Desired state resolved for one concrete target machine.

use serde::{Deserialize, Serialize};
use zup_core::{
    App, ComponentId, FileAssociationId, FileExtension, LauncherLocation, NonEmptyString,
    PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    PrerequisiteRequirement, Privilege, ProtocolScheme, RelativePath, ResourceKey, SelectedScope,
    ServiceId, ServiceStart, Sha256Digest, TargetTriple,
};

use crate::command::CommandSpec;
use crate::target_path::TargetPath;

/// Target-resolved plan. Contains no `Template` values and no build-machine paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlan {
    pub app: App,
    pub target: TargetTriple,
    pub scope: SelectedScope,
    pub install_directory: TargetPath,
    pub selected_components: Vec<ComponentId>,

    #[serde(default)]
    pub prerequisites: Vec<TargetPrerequisite>,
    pub files: Vec<TargetFile>,
    pub launchers: Vec<TargetLauncher>,
    pub path_entries: Vec<TargetPathEntry>,
    pub services: Vec<TargetService>,
    pub protocols: Vec<TargetProtocol>,
    pub file_associations: Vec<TargetFileAssociation>,

    pub summary: TargetPlanSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPrerequisite {
    pub id: PrerequisiteId,
    pub name: NonEmptyString,
    pub target: PrerequisiteArchitecture,
    pub requirement: PrerequisiteRequirement,
    pub package: PrerequisitePackage,
    pub installer: PrerequisiteInstaller,
}

/// One desired payload file on the target machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFile {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: TargetPath,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub privilege: Privilege,
}

/// One desired application launcher with a concrete launcher path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetLauncher {
    pub key: ResourceKey,
    pub location: LauncherLocation,
    pub name: NonEmptyString,
    /// Concrete launcher path on the target machine.
    pub launcher_path: TargetPath,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
    pub privilege: Privilege,
}

/// One desired search-path entry.
///
/// `scope` names the persistent search path that owns the entry (the per-user
/// or host-wide one). It is an independent decision from `privilege`: a
/// host-wide install may still add a per-user entry, and vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPathEntry {
    pub key: ResourceKey,
    pub value: TargetPath,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// One desired service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub name: NonEmptyString,
    pub display_name: Option<NonEmptyString>,
    pub command: CommandSpec,
    pub start: ServiceStart,
    pub privilege: Privilege,
}

/// One desired URI-protocol registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetProtocol {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub command: CommandSpec,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// One desired file-association registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFileAssociation {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileAssociationId,
    pub description: Option<String>,
    pub command: CommandSpec,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// Derived summary of a target plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlanSummary {
    pub file_count: usize,
    pub install_bytes: u64,
    pub resource_count: usize,
    /// True when at least one planned resource needs system authorization.
    pub requires_authorization: bool,
    pub selected_component_count: usize,
    pub prerequisite_count: usize,
    pub download_bytes: u64,
}
