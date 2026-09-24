//! Frontend-neutral runtime events and session state.

use serde::{Deserialize, Serialize};

/// Explicit session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Preparing,
    WaitingForElevation,
    ConnectingWorker,
    Executing,
    RollingBack,
    Completed,
    Cancelled,
    Failed,
}

/// Frontend-neutral events (CLI, GPUI, silent installer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuntimeEvent {
    StateChanged {
        state: RuntimeState,
    },
    WaitingForElevation,
    WorkerConnected,
    PreflightStarted,
    BlockingProcessesFound {
        detail: String,
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
}
