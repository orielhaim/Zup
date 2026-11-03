//! Typed resource changes and mutation preconditions.

use crate::observe::{
    ObservedExtensionState, ObservedFileAssociationState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState,
};
use crate::{CommandSpec, TargetPath};
use serde::{Deserialize, Serialize};
use zup_core::SelectedScope;
use zup_core::{Privilege, ProtocolScheme, RelativePath, ResourceKey, ServiceStart, Sha256Digest};

/// File payload decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperationKind {
    Create,
    Replace,
    RestoreOwned,
    RepairOwned,
    NoOp,
    Conflict,
    Drift,
}

/// Observed file state that must still hold when mutation begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FilePrecondition {
    Absent,
    Exact { size: u64, sha256: Sha256Digest },
}

/// Launcher decision (no Replace until ownership is proven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationChangeKind {
    Create,
    RepairOwned,
    UpdateOwned,
    RestoreOwned,
    NoOp,
    Conflict,
    Drift,
}

/// Search-path entry decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOperationKind {
    Add,
    Present,
    UpdateOwned,
    RestoreOwned,
    Conflict,
    Drift,
}

/// Typed conflict details attached to a `Conflict` operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Conflict {
    TargetNonFile {
        path: String,
    },
    File {
        path: String,
    },
    LauncherAlreadyOwnedByDifferentTarget {
        launcher_path: String,
        reason: String,
    },
    ServiceAlreadyExistsWithDifferentConfiguration {
        service: String,
        reason: String,
    },
    ProtocolAlreadyRegistered {
        scheme: String,
        reason: String,
    },
    FileAssociationConflict {
        id: String,
        reason: String,
    },
    FileExtensionConflict {
        extension: String,
        reason: String,
    },
    PathEntryConflict {
        value: String,
        reason: String,
    },
}

/// One file Create/Replace/NoOp/Conflict decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOperation {
    pub key: ResourceKey,
    pub kind: FileOperationKind,
    pub destination: TargetPath,
    pub source_relative: RelativePath,
    pub precondition: FilePrecondition,
    pub expected_sha256: Sha256Digest,
    pub expected_size: u64,
    pub privilege: Privilege,
    /// This file is intended to be executable. Portable intent, not a mode.
    #[serde(default)]
    pub executable: bool,
    pub conflict: Option<Conflict>,
}

/// One launcher decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherOperation {
    pub key: ResourceKey,
    pub kind: IntegrationChangeKind,
    pub launcher_path: TargetPath,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
    pub privilege: Privilege,
    pub previous: ObservedLauncherState,
    pub conflict: Option<Conflict>,
    /// File whose icon the shortcut should show. Absent when the shortcut
    /// should use its target's own icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<TargetPath>,
}

/// One search-path entry decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathOperation {
    pub key: ResourceKey,
    pub kind: PathOperationKind,
    pub value: TargetPath,
    /// Persistent search path that owns the entry, decided by the plan.
    pub scope: SelectedScope,
    pub privilege: Privilege,
    /// Whether the owning search path already held the entry at planning time.
    pub present: bool,
    pub previously_owned: bool,
    pub conflict: Option<Conflict>,
}

/// One service decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOperation {
    pub key: ResourceKey,
    pub kind: IntegrationChangeKind,
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub command: CommandSpec,
    pub start: ServiceStart,
    pub privilege: Privilege,
    pub previous: ObservedServiceState,
    pub conflict: Option<Conflict>,
}

/// One protocol registration decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolOperation {
    pub key: ResourceKey,
    pub kind: IntegrationChangeKind,
    pub scheme: ProtocolScheme,
    pub command: CommandSpec,
    pub previous: ObservedProtocolState,
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub conflict: Option<Conflict>,
}

/// One file-association decision (association and extension considered together).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAssociationOperation {
    pub key: ResourceKey,
    pub kind: IntegrationChangeKind,
    pub association_kind: IntegrationChangeKind,
    pub extension_kind: IntegrationChangeKind,
    pub extension: String,
    pub id: String,
    pub description: Option<String>,
    pub command: CommandSpec,
    pub previous_association: ObservedFileAssociationState,
    pub previous_extension: ObservedExtensionState,
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub conflict: Option<Conflict>,
}

/// Resource changes between validated desired and observed state.
///
/// Deterministic linear decisions - no execution DAG yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionPlan {
    pub binding: Option<crate::DeploymentBinding>,
    pub selected_components: Vec<zup_core::ComponentId>,
    #[serde(default)]
    pub install_directory: Option<TargetPath>,
    pub uninstall: bool,
    pub removals: Vec<RemovalOperation>,
    pub files: Vec<FileOperation>,
    pub launchers: Vec<LauncherOperation>,
    pub path_entries: Vec<PathOperation>,
    pub services: Vec<ServiceOperation>,
    pub protocols: Vec<ProtocolOperation>,
    pub file_associations: Vec<FileAssociationOperation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    RemoveOwned,
    Drift,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalOperation {
    pub key: ResourceKey,
    pub kind: RemovalKind,
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub owned: crate::OwnedResource,
}
