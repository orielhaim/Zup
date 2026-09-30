//! How an operation is going, what it would change, and what went wrong.

use serde::{Deserialize, Serialize};

use crate::{ComponentId, InstallScope};

/// The stage an operation has reached.
///
/// A coarse position in the run rather than a step list: a preset draws a
/// timeline from these, and a finer vocabulary would be a layout decision the
/// protocol has no business making.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Prepare,
    Download,
    Verify,
    Files,
    System,
    Finish,
}

/// How far along the current operation is.
///
/// A `total` of zero means the work is not countable yet, which is a different
/// thing from zero percent and is why the ratio is optional rather than
/// defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressPresentation {
    pub phase: OperationPhase,
    pub completed: u64,
    pub total: u64,
    pub label: String,
}

impl ProgressPresentation {
    pub fn new(
        phase: OperationPhase,
        completed: u64,
        total: u64,
        label: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            completed,
            total,
            label: label.into(),
        }
    }

    /// Completion as a whole percentage, or `None` while the work is not countable.
    pub fn percent(&self) -> Option<u32> {
        (self.total > 0)
            .then(|| ((self.completed.min(self.total) as f64 / self.total as f64) * 100.0) as u32)
    }
}

/// What kind of machine state a change touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCategory {
    Files,
    Launchers,
    Path,
    Services,
    Protocols,
    FileAssociations,
    AppsFeatures,
    Maintenance,
    Prerequisites,
    Other,
}

/// What an operation would do to one resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Create,
    Update,
    Remove,
    NoChange,
    Drifted,
    Conflict,
}

/// One resource an operation would touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedChange {
    pub category: ResourceCategory,
    pub kind: ChangeKind,
    pub label: String,
    pub location: Option<String>,
    pub scope: Option<InstallScope>,
    /// True when this single change needs host-wide authority.
    pub requires_authorization: bool,
    pub estimated_bytes: u64,
    pub component: Option<ComponentId>,
}

/// The changes that touch one category, in the order they would happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeGroup {
    pub category: ResourceCategory,
    pub changes: Vec<PlannedChange>,
}

impl ChangeGroup {
    pub fn total_bytes(&self) -> u64 {
        self.changes.iter().fold(0u64, |total, change| {
            total.saturating_add(change.estimated_bytes)
        })
    }
}

/// Whether a declared requirement is already satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Satisfied,
    Missing,
    Unknown,
}

/// A system dependency the application declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementPresentation {
    pub id: String,
    pub name: String,
    pub status: RequirementStatus,
    pub estimated_bytes: u64,
    /// True when other applications may also depend on it.
    pub shared: bool,
}

/// What an operation would do to this machine.
///
/// The answer to "what will change", not a transaction: there is no node graph,
/// no precondition, and nothing here is executable. It is the cost and the blast
/// radius, which is what a person is deciding on when they look at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub scope: InstallScope,
    pub install_directory: String,
    pub selected_components: Vec<ComponentId>,
    pub estimated_bytes: u64,
    pub download_bytes: u64,
    /// True when any change in this preview needs host-wide authority.
    pub requires_authorization: bool,
    pub groups: Vec<ChangeGroup>,
    pub requirements: Vec<RequirementPresentation>,
}

/// Why an operation stopped, in the shape a person can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Blocked,
    Conflict,
    Drift,
    Permission,
    Recovery,
    Verification,
    Busy,
    Unknown,
}

/// An explanation of a failure, written for the person who has to resolve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticPresentation {
    pub kind: DiagnosticKind,
    pub title: String,
    pub meaning: String,
    pub recovery: String,
    pub technical_details: Option<String>,
}

/// Whether a maintenance surface is up to date, and by how much it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "health")]
pub enum InstallationHealth {
    /// The installation has not been inspected yet.
    Unknown,
    /// Every managed resource still matches what was installed.
    UpToDate,
    /// These managed resources no longer match, and a repair would leave them alone.
    Drifted { resources: Vec<String> },
}

/// Where an update has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum UpdateState {
    /// No check has run in this session.
    Idle,
    /// A check is in flight, and this is what it is doing.
    Checking { detail: String },
    /// A newer release has been found and is being installed.
    Installing { detail: String },
    /// The installed version is the newest one on the channel.
    UpToDate { current: String },
    /// A newer release exists.
    Available { current: String, available: String },
    /// The check or the update itself failed.
    Failed { message: String },
}

/// The update situation, when the application is configured for updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePresentation {
    pub channel: Option<String>,
    pub state: UpdateState,
}

const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// A byte count as a person reads it.
///
/// Every preset needs this and the number is the protocol's own, so the
/// rendering belongs beside it rather than in each preset.
pub fn format_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < BYTE_UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", BYTE_UNITS[unit])
    }
}
