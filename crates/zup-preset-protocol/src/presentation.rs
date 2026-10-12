use serde::{Deserialize, Serialize};

use crate::{ComponentId, InstallScope};

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

    pub fn percent(&self) -> Option<u32> {
        (self.total > 0)
            .then(|| ((self.completed.min(self.total) as f64 / self.total as f64) * 100.0) as u32)
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedChange {
    pub category: ResourceCategory,
    pub kind: ChangeKind,
    pub label: String,
    pub location: Option<String>,
    pub scope: Option<InstallScope>,
    pub requires_authorization: bool,
    pub estimated_bytes: u64,
    pub component: Option<ComponentId>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Satisfied,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementPresentation {
    pub id: String,
    pub name: String,
    pub status: RequirementStatus,
    pub estimated_bytes: u64,
    pub shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub scope: InstallScope,
    pub install_directory: String,
    pub selected_components: Vec<ComponentId>,
    pub estimated_bytes: u64,
    pub download_bytes: u64,
    pub requires_authorization: bool,
    pub groups: Vec<ChangeGroup>,
    pub requirements: Vec<RequirementPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PlanStatus {
    Unsupported,
    Computing { last: Option<Box<PlanPreview>> },
    Ready { preview: Box<PlanPreview> },
    Failed { reason: String },
}

impl PlanStatus {
    pub fn latest(&self) -> Option<&PlanPreview> {
        match self {
            Self::Ready { preview } => Some(preview),
            Self::Computing { last } => last.as_deref(),
            Self::Unsupported | Self::Failed { .. } => None,
        }
    }

    pub fn current(&self) -> Option<&PlanPreview> {
        match self {
            Self::Ready { preview } => Some(preview),
            _ => None,
        }
    }

    pub fn is_computing(&self) -> bool {
        matches!(self, Self::Computing { .. })
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticPresentation {
    pub kind: DiagnosticKind,
    pub title: String,
    pub meaning: String,
    pub recovery: String,
    pub technical_details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "health")]
pub enum InstallationHealth {
    Unknown,
    UpToDate,
    Drifted { resources: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum UpdateState {
    Idle,
    Checking { detail: String },
    Installing { detail: String },
    UpToDate { current: String },
    Available { current: String, available: String },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePresentation {
    pub channel: Option<String>,
    pub state: UpdateState,
}

const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{:.0} {}", value, BYTE_UNITS[unit])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, BYTE_UNITS[unit])
    } else {
        format!("{:.2} {}", value, BYTE_UNITS[unit])
    }
}
