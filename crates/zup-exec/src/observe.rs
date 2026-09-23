//! Immutable observation of target-machine resource state.
//!
//! Inspection reports facts only. Delta decisions live in [`crate::plan`].

use serde::{Deserialize, Serialize};
use zup_core::{ProtocolScheme, ResourceKey, SelectedScope, ServiceId, ServiceStart, Sha256Digest};
use zup_platform::{CommandSpec, TargetPath};

/// Observed state of one desired target file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedFile {
    pub key: ResourceKey,
    pub path: TargetPath,
    pub state: ObservedFileState,
}

/// What occupies a target path at inspection time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFileState {
    Absent,
    File { size: u64, sha256: Sha256Digest },
    NonFile,
}

/// Observed state of one desired shortcut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedShortcut {
    pub key: ResourceKey,
    pub link_path: TargetPath,
    pub state: ObservedShortcutState,
}

/// Semantic content of an existing `.lnk`, or why it cannot be read as one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedShortcutState {
    Absent,
    Shortcut {
        target: TargetPath,
        arguments: Vec<String>,
        working_directory: Option<TargetPath>,
    },
    InvalidShortcut,
    NonFile,
}

/// Observed state of one desired PATH entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedPathEntry {
    pub key: ResourceKey,
    pub desired: TargetPath,
    pub scope: SelectedScope,
    pub state: PathEntryState,
}

/// Whether the persistent PATH already contains a matching entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathEntryState {
    Absent,
    Present {
        /// Raw PATH segment as stored on the machine.
        raw_entry: String,
    },
}

/// Observed state of one desired service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub state: ObservedServiceState,
}

/// SCM-observed service configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedServiceState {
    Absent,
    Service {
        display_name: String,
        command: CommandSpec,
        start: ServiceStart,
        runtime_state: Option<ServiceRuntimeState>,
    },
}

/// Running/stopped is diagnostics only — not desired configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRuntimeState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    Unknown,
}

/// Observed state of one desired URI protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedProtocol {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub scope: SelectedScope,
    pub state: ObservedProtocolState,
}

/// Registry-observed protocol handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedProtocolState {
    Absent,
    Registration {
        command: CommandSpec,
        url_protocol_marker: bool,
    },
    Malformed {
        reason: String,
    },
}

/// Observed file-type ProgID and extension mapping (tracked independently).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedFileType {
    pub key: ResourceKey,
    pub extension: String,
    pub id: String,
    pub scope: SelectedScope,
    pub id_state: ObservedProgIdState,
    pub extension_state: ObservedExtensionState,
}

/// Observed ProgID registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedProgIdState {
    Absent,
    Registration {
        description: Option<String>,
        command: CommandSpec,
    },
    Malformed {
        reason: String,
    },
}

/// Observed `.ext` → ProgID mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedExtensionState {
    Absent,
    Mapped {
        /// Default ProgID currently associated with the extension.
        prog_id: String,
    },
    Malformed {
        reason: String,
    },
}

/// Immutable snapshot of observed machine state for planned resources.
///
/// Every collection follows `TargetPlan` ordering. Inspection never mutates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MachineSnapshot {
    pub files: Vec<ObservedFile>,
    pub shortcuts: Vec<ObservedShortcut>,
    pub path_entries: Vec<ObservedPathEntry>,
    pub services: Vec<ObservedService>,
    pub protocols: Vec<ObservedProtocol>,
    pub file_types: Vec<ObservedFileType>,
}
