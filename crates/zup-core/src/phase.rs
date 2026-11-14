use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Prepare,
    Download,
    Verify,
    Stage,
    Activate,
    Rollback,
    Recovery,
    Cleanup,
    Files,
    System,
    Finish,
}

impl OperationPhase {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Prepare => "Prepare",
            Self::Download => "Download",
            Self::Verify => "Verify",
            Self::Stage => "Stage application",
            Self::Activate => "Activate",
            Self::Rollback => "Restore previous deployment",
            Self::Recovery => "Recover installation",
            Self::Cleanup => "Clean up",
            Self::Files => "Files",
            Self::System => "System",
            Self::Finish => "Finish",
        }
    }
}
