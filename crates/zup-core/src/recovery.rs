use serde::{Deserialize, Serialize};

use crate::{AppId, InstallationId, SelectedScope, Target, TargetPath, TransactionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    CommittedCleanup,
    CommittedReconciliation,
    Interrupted,
    PublicationUncertain,
    OperatorRequired,
}

/// Presentation only. Native recovery must reload the protected transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySummary {
    pub transaction: TransactionId,
    pub installation: InstallationId,
    pub app: AppId,
    pub version: semver::Version,
    pub scope: SelectedScope,
    pub target: Target,
    pub directory: Option<TargetPath>,
    pub kind: RecoveryKind,
    pub detail: String,
    pub cleanup: Vec<RecoveryCleanup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryCleanup {
    Generation { directory: TargetPath },
    FixedFile { path: TargetPath },
    Metadata { transaction: TransactionId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentSettlement {
    Committed,
    RolledBack,
}

/// Read-only projection; authenticated inspection must recreate it before launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveLaunchers {
    pub transaction: TransactionId,
    pub installation: InstallationId,
    pub app: AppId,
    pub scope: SelectedScope,
    pub target: Target,
    pub launchers: Vec<ActiveLauncher>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveLauncher {
    pub name: String,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
}
