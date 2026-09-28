//! Immutable observation of target-host resource state.
//!
//! Inspection reports facts only. Delta decisions live in [`crate::plan`].
//! Nothing here names an operating-system facility: a service is a service
//! wherever it lives, and a registration is a registration.

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

/// Observed state of one desired launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedLauncher {
    pub key: ResourceKey,
    pub launcher_path: TargetPath,
    pub state: ObservedLauncherState,
}

/// Semantic content of an existing launcher, or why it cannot be read as one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedLauncherState {
    Absent,
    Launcher {
        target: TargetPath,
        arguments: Vec<String>,
        working_directory: Option<TargetPath>,
    },
    InvalidLauncher,
    NonFile,
}

/// One persistent executable search path, as the host stores it.
///
/// Entries arrive already split and normalized to [`TargetPath`], so a platform
/// adapter owns the separator, quoting, and case rules of its own host format.
/// This crate only asks whether a desired entry is in the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SearchPath {
    entries: Vec<TargetPath>,
}

impl SearchPath {
    pub fn new(entries: Vec<TargetPath>) -> Self {
        Self { entries }
    }

    /// Stored entries, in stored order.
    pub fn entries(&self) -> &[TargetPath] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Set-membership decision for one entry.
    pub fn contains(&self, entry: &TargetPath) -> bool {
        self.entries.iter().any(|stored| stored.equivalent(entry))
    }
}

impl FromIterator<TargetPath> for SearchPath {
    fn from_iter<I: IntoIterator<Item = TargetPath>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a SearchPath {
    type Item = &'a TargetPath;
    type IntoIter = std::slice::Iter<'a, TargetPath>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

/// Observed state of one desired search-path entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedPathEntry {
    pub key: ResourceKey,
    pub desired: TargetPath,
    /// Persistent search path that owns `desired`.
    pub scope: SelectedScope,
    /// Contents of that search path at inspection time.
    pub search_path: SearchPath,
}

/// Observed state of one desired service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub state: ObservedServiceState,
}

/// Service registration and configuration, as the host reports it.
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

/// Running/stopped is diagnostics only - not desired configuration.
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

/// Protocol handler registration, as the host reports it.
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

/// Observed file-association and extension mapping (tracked independently).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedFileAssociation {
    pub key: ResourceKey,
    pub extension: String,
    pub id: String,
    pub scope: SelectedScope,
    pub association_state: ObservedFileAssociationState,
    pub extension_state: ObservedExtensionState,
}

/// File-association registration, as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFileAssociationState {
    Absent,
    Registration {
        description: Option<String>,
        command: CommandSpec,
    },
    Malformed {
        reason: String,
    },
}

/// Extension-to-association mapping, as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedExtensionState {
    Absent,
    Mapped {
        /// Logical association currently mapped to the extension.
        association_id: String,
    },
    Malformed {
        reason: String,
    },
}

/// Immutable snapshot of observed host state for planned resources.
///
/// Every collection follows `TargetPlan` ordering. Inspection never mutates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostSnapshot {
    pub files: Vec<ObservedFile>,
    pub launchers: Vec<ObservedLauncher>,
    pub path_entries: Vec<ObservedPathEntry>,
    pub services: Vec<ObservedService>,
    pub protocols: Vec<ObservedProtocol>,
    pub file_associations: Vec<ObservedFileAssociation>,
}
