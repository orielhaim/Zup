//! Desired state resolved for one concrete target machine.

use serde::{Deserialize, Serialize};
use zup_core::{
    ActionId, ActionKind, App, ComponentId, FileExtension, FileTypeId, NonEmptyString, Privilege,
    ProtocolScheme, RelativePath, ResourceKey, SelectedScope, ServiceId, ServiceStart,
    Sha256Digest, ShortcutLocation,
};

use crate::command::CommandSpec;
use crate::target_path::TargetPath;

/// Target-resolved plan. Contains no `Template` values and no build-machine paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlan {
    pub app: App,
    pub scope: SelectedScope,
    pub install_directory: TargetPath,
    pub selected_components: Vec<ComponentId>,

    pub files: Vec<TargetFile>,
    pub shortcuts: Vec<TargetShortcut>,
    pub path_entries: Vec<TargetPathEntry>,
    pub services: Vec<TargetService>,
    pub protocols: Vec<TargetProtocol>,
    pub file_types: Vec<TargetFileType>,
    pub actions: Vec<TargetExternalAction>,

    pub summary: TargetPlanSummary,
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

/// One desired application shortcut with a concrete `.lnk` path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetShortcut {
    pub key: ResourceKey,
    pub location: ShortcutLocation,
    pub name: NonEmptyString,
    /// Concrete `.lnk` path on the target machine.
    pub link_path: TargetPath,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
    pub privilege: Privilege,
}

/// One desired PATH entry.
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

/// One desired file-type registration (ProgID + extension mapping).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFileType {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileTypeId,
    pub description: Option<String>,
    pub command: CommandSpec,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// One desired opaque external action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetExternalAction {
    pub key: ResourceKey,
    pub id: ActionId,
    pub kind: ActionKind,
    pub apply: CommandSpec,
    pub rollback: Option<CommandSpec>,
    pub uninstall: Option<CommandSpec>,
    pub privilege: Privilege,
    pub opaque: bool,
}

/// Derived summary of a target plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlanSummary {
    pub file_count: usize,
    pub install_bytes: u64,
    pub resource_count: usize,
    pub requires_elevation: bool,
    pub selected_component_count: usize,
}
