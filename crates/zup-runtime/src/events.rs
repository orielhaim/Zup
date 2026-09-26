//! Frontend-neutral runtime events and session state.

use serde::{Deserialize, Serialize};

/// Explicit session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Preparing,
    CheckingPrerequisites,
    InstallingPrerequisites,
    WaitingForAuthorization,
    RebootRequired,
    ConnectingWorker,
    Executing,
    RollingBack,
    Completed,
    Cancelled,
    Failed,
}

/// Frontend-neutral events (CLI, native UI, silent installer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuntimeEvent {
    StateChanged {
        state: RuntimeState,
    },
    WaitingForAuthorization,
    WorkerConnected,
    PrerequisiteCheck {
        id: String,
        name: String,
        satisfied: bool,
        version: Option<String>,
    },
    PrerequisiteDownload {
        id: String,
        completed: u64,
        total: Option<u64>,
    },
    PrerequisiteInstall {
        id: String,
        name: String,
    },
    RebootRequired {
        id: String,
        exit_code: i32,
    },
    PreflightStarted,
    ResourceBlocked {
        detail: String,
        pids: Vec<u32>,
    },
    StagingStarted {
        id: String,
    },
    StagingProgress {
        id: String,
        detail: String,
    },
    OperationStarted {
        id: String,
    },
    Progress {
        completed: u64,
        total: u64,
        action: String,
    },
    RollingBack,
    Completed {
        outcome: String,
    },
    Failed {
        kind: String,
        message: String,
    },
    LogPath {
        path: String,
    },
}
