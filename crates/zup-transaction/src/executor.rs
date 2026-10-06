use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zup_core::{ResourceKey, Sha256Digest};
use zup_platform::TargetPath;

use crate::plan::TransactionNode;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReconcileResult {
    #[default]
    NotApplied,
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
        /// Whether the installed file is executable.
        ///
        /// Recorded rather than assumed, because the two backends satisfy the
        /// intent by different means: a backend with filesystem modes sets a bit
        /// it can re-read, and a backend where an executable is identifiable by
        /// its form has nothing to set and reports what it delivered. A receipt
        /// that omitted this would let a transaction report success for a file
        /// that is not runnable, and reconcile would have nothing to compare
        /// against.
        executable: bool,
        created_directories: Vec<String>,
    },
    ReplaceFile {
        destination: String,
        previous_sha256: String,
        previous_size: u64,
        backup_path: String,
        new_sha256: String,
        new_size: u64,
        /// Whether the installed file is executable. See [`CreateFile`].
        executable: bool,
    },
    RemoveFile {
        destination: TargetPath,
        backup_path: TargetPath,
        sha256: Sha256Digest,
        size: u64,
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
                ..
            } => {
                Sha256Digest::from_str(previous_sha256)
                    .and_then(|_| Sha256Digest::from_str(new_sha256))
                    .map_err(|_| "invalid replaced file digest".to_string())?;
            }
            Self::RemoveFile { .. } | Self::Control => {}
        }
        Ok(())
    }
}

pub trait OperationExecutor {
    type Error;

    /// Preflight one node before the transaction mutates anything.
    ///
    /// Called for the `Begin` and `Preflight` barriers, so it must observe
    /// only: a prepare that changes state defeats the barrier it guards.
    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error>;

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error>;

    /// Confirm that an applied node's installed state matches its receipt.
    ///
    /// Called once per applied mutation, file removal, and backend operation
    /// before the commit barrier. The receipt is the only durable record of
    /// what the apply was supposed to install, so it is the comparison basis.
    /// Must observe only.
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
