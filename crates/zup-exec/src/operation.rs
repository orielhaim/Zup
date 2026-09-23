//! Execution operations, conflicts, and summary.

use crate::observe::{
    ObservedExtensionState, ObservedProgIdState, ObservedProtocolState, ObservedServiceState,
    ObservedShortcutState, PathEntryState,
};
use serde::{Deserialize, Serialize};
use zup_core::{
    ActionId, ActionKind, Privilege, ProtocolScheme, RelativePath, ResourceKey, ServiceStart,
    Sha256Digest,
};
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
    /// Opaque external actions cannot be reasoned about.
    RunOpaque,
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

/// Shortcut decision (no Replace until ownership is proven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutOperationKind {
    Create,
    UpdateOwned,
    RestoreOwned,
    NoOp,
    Conflict,
    Drift,
}

/// PATH entry decision.
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

/// File-type ProgID/extension decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTypeOperationKind {
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
    TargetNonFile { path: String },
    File { path: String },
    ShortcutAlreadyOwnedByDifferentTarget { link_path: String, reason: String },
    ServiceAlreadyExistsWithDifferentConfiguration { service: String, reason: String },
    ProtocolAlreadyRegistered { scheme: String, reason: String },
    FileTypeProgIdConflict { id: String, reason: String },
    FileExtensionConflict { extension: String, reason: String },
    PathEntryConflict { value: String, reason: String },
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
    pub conflict: Option<Conflict>,
}

/// One shortcut decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutOperation {
    pub key: ResourceKey,
    pub kind: ShortcutOperationKind,
    pub link_path: TargetPath,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
    pub previous: ObservedShortcutState,
    pub conflict: Option<Conflict>,
}

/// One PATH entry decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathOperation {
    pub key: ResourceKey,
    pub kind: PathOperationKind,
    pub value: TargetPath,
    pub scope: SelectedScope,
    pub previous: PathEntryState,
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
    pub conflict: Option<Conflict>,
}

/// One file-type decision (ProgID + extension considered together).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTypeOperation {
    pub key: ResourceKey,
    pub kind: FileTypeOperationKind,
    pub prog_id_kind: FileTypeOperationKind,
    pub extension_kind: FileTypeOperationKind,
    pub extension: String,
    pub id: String,
    pub description: Option<String>,
    pub command: CommandSpec,
    pub previous_id: ObservedProgIdState,
    pub previous_extension: ObservedExtensionState,
    pub scope: SelectedScope,
    pub conflict: Option<Conflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedOperation {
    Shortcut(ShortcutOperation),
    Path(PathOperation),
    Service(ServiceOperation),
    Protocol(ProtocolOperation),
    ProgId(FileTypeOperation),
    Extension(FileTypeOperation),
}

/// Opaque external action execution node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActionOperation {
    pub key: ResourceKey,
    pub id: ActionId,
    pub kind: ActionKind,
    pub apply: CommandSpec,
    pub rollback: Option<CommandSpec>,
    pub uninstall: Option<CommandSpec>,
    pub privilege: Privilege,
    pub opaque: bool,
}

/// Derived execution summary with checked counts and byte totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionSummary {
    pub files_create: usize,
    pub files_replace: usize,
    pub files_unchanged: usize,
    pub files_conflict: usize,

    pub shortcuts_create: usize,
    pub shortcuts_unchanged: usize,
    pub shortcuts_conflict: usize,

    pub path_entries_add: usize,
    pub path_entries_present: usize,
    pub path_entries_conflict: usize,

    pub services_create: usize,
    pub services_unchanged: usize,
    pub services_conflict: usize,

    pub protocols_create: usize,
    pub protocols_unchanged: usize,
    pub protocols_conflict: usize,

    pub file_types_create: usize,
    pub file_types_unchanged: usize,
    pub file_types_conflict: usize,

    pub opaque_actions: usize,

    pub write_bytes: u64,
    pub total_desired_bytes: u64,
    pub requires_elevation: bool,
}

/// Full v1 file-and-resource delta between desired and observed state.
///
/// Deterministic linear decisions — no execution DAG yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionPlan {
    pub selected_components: Vec<zup_core::ComponentId>,
    pub uninstall: bool,
    pub removals: Vec<RemovalOperation>,
    pub files: Vec<FileOperation>,
    pub shortcuts: Vec<ShortcutOperation>,
    pub path_entries: Vec<PathOperation>,
    pub services: Vec<ServiceOperation>,
    pub protocols: Vec<ProtocolOperation>,
    pub file_types: Vec<FileTypeOperation>,
    pub external_actions: Vec<ExternalActionOperation>,
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
    pub owned: crate::OwnedResource,
}
