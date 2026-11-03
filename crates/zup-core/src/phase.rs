use serde::{Deserialize, Serialize};

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

impl OperationPhase {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Prepare => "Prepare",
            Self::Download => "Download",
            Self::Verify => "Verify",
            Self::Files => "Files",
            Self::System => "System",
            Self::Finish => "Finish",
        }
    }
}
