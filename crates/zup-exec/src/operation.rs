//! Execution operations, conflicts, and summary.

use crate::observe::{
    ObservedExtensionState, ObservedFileAssociationState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState,
};
use serde::{Deserialize, Serialize};
use zup_core::{Privilege, ProtocolScheme, RelativePath, ResourceKey, ServiceStart, Sha256Digest};
use zup_platform::{CommandSpec, SelectedScope, TargetPath};

/// High-level comparison result for one desired resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delta {
    Create,
    /// Files only: overwrite a differing regular file.
    Replace,
    RestoreOwned,
    RepairOwned,
    NoOp,
    Conflict,
}

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
pub enum LauncherOperationKind {
    Create,
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

/// Service decision (no reconfigure until ownership is proven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationKind {
    Create,
    UpdateOwned,
    RestoreOwned,
    NoOp,
    Conflict,
    Drift,
}

/// Protocol registration decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolOperationKind {
    Create,
    UpdateOwned,
    RestoreOwned,
    NoOp,
    Conflict,
    Drift,
}

/// File-association decision (association and extension considered together).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAssociationOperationKind {
    Create,
    UpdateOwned,
    RestoreOwned,
    NoOp,
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
    pub conflict: Option<Conflict>,
}

/// One launcher decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherOperation {
    pub key: ResourceKey,
    pub kind: LauncherOperationKind,
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
    pub kind: ServiceOperationKind,
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
    pub kind: ProtocolOperationKind,
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
    pub kind: FileAssociationOperationKind,
    pub association_kind: FileAssociationOperationKind,
    pub extension_kind: FileAssociationOperationKind,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedOperation {
    Launcher(LauncherOperation),
    Path(PathOperation),
    Service(ServiceOperation),
    Protocol(ProtocolOperation),
    FileAssociation(FileAssociationOperation),
    Extension(FileAssociationOperation),
}

/// Derived execution summary with checked counts and byte totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionSummary {
    pub files_create: usize,
    pub files_replace: usize,
    pub files_unchanged: usize,
    pub files_conflict: usize,

    pub launchers_create: usize,
    pub launchers_unchanged: usize,
    pub launchers_conflict: usize,

    pub path_entries_add: usize,
    pub path_entries_present: usize,
    pub path_entries_conflict: usize,

    pub services_create: usize,
    pub services_unchanged: usize,
    pub services_conflict: usize,

    pub protocols_create: usize,
    pub protocols_unchanged: usize,
    pub protocols_conflict: usize,

    pub file_associations_create: usize,
    pub file_associations_unchanged: usize,
    pub file_associations_conflict: usize,

    pub write_bytes: u64,
    pub total_desired_bytes: u64,
    /// True when at least one planned operation needs system authorization.
    pub requires_authorization: bool,
}

/// Full v1 file-and-resource delta between desired and observed state.
///
/// Deterministic linear decisions - no execution DAG yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionPlan {
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
    pub summary: ExecutionSummary,
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
