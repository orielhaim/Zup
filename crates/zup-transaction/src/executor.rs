use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zup_core::{ResourceKey, Sha256Digest};
use zup_platform::TargetPath;

use crate::plan::TransactionNode;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReconcileResult {
    #[default]
    NotApplied,
    /// Safe to resume, but effects remain and cannot be skipped during rollback.
    Incomplete,
    Applied,
    AppliedWithReceipt(OperationReceipt),
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationReceipt {
    Control,
    StageFile {
        staged_path: String,
        size: u64,
        sha256: String,
    },
    CreateFile {
        destination: String,
        installed_sha256: String,
        installed_size: u64,
        executable: bool,
        created_directories: Vec<String>,
    },
    ReplaceFile {
        destination: String,
        previous_sha256: String,
        previous_size: u64,
        previous_mode: u32,
        backup_path: String,
        new_sha256: String,
        new_size: u64,
        executable: bool,
    },
    RemoveFile {
        destination: TargetPath,
        backup_path: TargetPath,
        sha256: Sha256Digest,
        size: u64,
        mode: u32,
    },
    Backend {
        key: ResourceKey,
        payload: Vec<u8>,
    },
}

impl OperationReceipt {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Backend { key, payload } => {
                if !matches!(key, ResourceKey::Backend { .. }) {
                    return Err("backend receipt key is not a backend resource".into());
                }
                if payload.len() > crate::MAX_BACKEND_PAYLOAD_BYTES {
                    return Err("backend receipt payload exceeds the transaction limit".into());
                }
            }
            Self::StageFile { sha256, .. } => {
                Sha256Digest::from_str(sha256)
                    .map_err(|_| "invalid staged file digest".to_string())?;
            }
            Self::CreateFile {
                installed_sha256, ..
            } => {
                Sha256Digest::from_str(installed_sha256)
                    .map_err(|_| "invalid installed file digest".to_string())?;
            }
            Self::ReplaceFile {
                previous_sha256,
                new_sha256,
                previous_mode,
                ..
            } => {
                Sha256Digest::from_str(previous_sha256)
                    .and_then(|_| Sha256Digest::from_str(new_sha256))
                    .map_err(|_| "invalid replaced file digest".to_string())?;
                if *previous_mode & !0o7777 != 0 {
                    return Err("invalid replaced file mode".to_string());
                }
            }
            Self::RemoveFile { mode, .. } => {
                if *mode & !0o7777 != 0 {
                    return Err("invalid removed file mode".to_string());
                }
            }
            Self::Control => {}
        }
        Ok(())
    }
}

pub trait OperationExecutor {
    type Error;

    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error>;

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error>;

    fn verify(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error>;

    fn rollback(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error>;

    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error>;
}

pub trait CancellationProbe {
    fn is_cancelled(&self) -> bool;
}
