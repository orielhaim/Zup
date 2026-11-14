//! Typed resource changes and mutation preconditions.

use crate::observe::{
    ObservedExtensionState, ObservedFileAssociationState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState,
};
use serde::{Deserialize, Serialize};
use zup_core::SelectedScope;
use zup_core::{CommandSpec, TargetPath};
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
    Exact {
        object: zup_core::NativeObjectIdentity,
        size: u64,
        sha256: Sha256Digest,
    },
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
    RebindOwned,
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
    pub replacement: zup_core::FixedReplacement,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathBinding {
    pub value: TargetPath,
    pub value_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathPresence {
    Missing,
    Single,
    Multiple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ownership", rename_all = "snake_case", deny_unknown_fields)]
pub enum PathPrecondition {
    Unclaimed {
        presence: PathPresence,
    },
    Owned {
        binding: PathBinding,
        previous: PathPresence,
        desired: PathPresence,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathOperation {
    pub key: ResourceKey,
    pub kind: PathOperationKind,
    pub value: TargetPath,
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub precondition: PathPrecondition,
    pub conflict: Option<Conflict>,
}

/// One service decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOperation {
    pub state_directories: Vec<zup_core::RelativePath>,
    pub registration: zup_core::NativeServiceRegistration,
    pub failure: zup_core::ServiceFailurePolicy,
    pub execution: zup_core::ServiceExecution,
    pub key: ResourceKey,
    pub kind: IntegrationChangeKind,
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub command: CommandSpec,
    pub start: ServiceStart,
    pub lifecycle: zup_core::ServiceLifecycle,
    pub readiness: zup_core::ServiceReadiness,
    pub requires_files: Vec<crate::TargetPath>,
    pub requires_services: Vec<zup_core::ServiceId>,
    pub dependency_names: Vec<String>,
    pub privilege: Privilege,
    pub previous: ObservedServiceState,
    pub conflict: Option<Conflict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServiceCompensationError {
    #[error(
        "repair displaced an externally modified service configuration; retain the repaired registration and recover before resuming its previous runtime"
    )]
    Repair,
    #[error("the previous service configuration is unrecognized and cannot be restored safely")]
    UnrecognizedConfiguration,
}

impl ServiceOperation {
    pub fn validate_compensation(&self) -> Result<(), ServiceCompensationError> {
        if self.kind == IntegrationChangeKind::RepairOwned {
            return Err(ServiceCompensationError::Repair);
        }
        if self.previous.configuration().is_none() {
            return Err(ServiceCompensationError::UnrecognizedConfiguration);
        }
        Ok(())
    }

    pub fn preserves_runtime(&self) -> bool {
        self.desired_configuration()
            .update_from(
                &self
                    .previous
                    .configuration()
                    .unwrap_or(crate::ServiceState::Absent),
            )
            .preserves_runtime()
    }

    pub fn desired_configuration(&self) -> crate::ServiceState {
        crate::ServiceState::Registration {
            state_directories: self.state_directories.clone(),
            native_timeouts: self.lifecycle.timeouts_for(&self.execution).native(),
            dependencies: self.dependency_names.clone(),
            failure: self.failure,
            execution: self.execution.clone(),
            readiness: self.readiness,
            display_name: self.display_name.clone(),
            command: Box::new(self.command.clone()),
            start: self.start,
        }
    }
    pub fn affected_by_files(
        &self,
        files: &[FileOperation],
        removals: &[RemovalOperation],
        owned: Option<&crate::OwnedResource>,
    ) -> bool {
        let previous = match owned {
            Some(crate::OwnedResource::Service(crate::OwnedService { requires_files, .. })) => {
                requires_files.as_slice()
            }
            _ => &[],
        };
        std::iter::once(&self.command.executable)
            .chain(&self.requires_files)
            .chain(previous)
            .any(|path| files.iter().any(|file| &file.destination == path && file.kind != FileOperationKind::NoOp)
                || removals.iter().any(|removal| matches!(&removal.owned, crate::OwnedResource::File { destination, .. } if destination == path)))
    }
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
    pub transition: crate::DeploymentTransition,
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
