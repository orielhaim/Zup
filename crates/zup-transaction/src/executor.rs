//! Operation executor interface (synchronous) with typed receipts.

use serde::{Deserialize, Serialize};

use crate::plan::TransactionNode;
use zup_core::{SelectedScope, Sha256Digest};
use zup_exec::{
    ExtensionState, ProgIdState, ProtocolState, ServiceState, ShortcutState, UninstallEntryState,
};
use zup_platform::TargetPath;

/// Result of reconciling a node whose durable state is `Running`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReconcileResult {
    /// Side effect did not land. Retry is safe.
    #[default]
    NotApplied,
    /// Side effect landed. Record `Applied` and continue.
    Applied,
    AppliedWithReceipt(OperationReceipt),
    /// Outcome unknown. Do not guess.
    Ambiguous,
}

/// Typed durable receipt for one applied operation.
///
/// Journaled as part of `Applied`. Unknown future variants must fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationReceipt {
    /// Control barrier (begin, preflight, commit-intent, verify, commit).
    Control,
    /// Payload staged to the transaction work area.
    StageFile {
        staged_path: String,
        size: u64,
        sha256: String,
    },
    /// File created at the destination.
    CreateFile {
        destination: String,
        installed_sha256: String,
        installed_size: u64,
        created_directories: Vec<String>,
    },
    /// File replaced; previous content preserved under `backup_path`.
    ReplaceFile {
        destination: String,
        previous_sha256: String,
        previous_size: u64,
        backup_path: String,
        new_sha256: String,
        new_size: u64,
    },
    RemoveFile {
        destination: TargetPath,
        backup_path: TargetPath,
        sha256: Sha256Digest,
        size: u64,
    },
    PathEntry {
        scope: SelectedScope,
        entry: String,
        value_type: String,
    },
    RemovePathEntry {
        scope: SelectedScope,
        entry: String,
        value_type: String,
    },
    Shortcut {
        link_path: TargetPath,
        previous: Box<ShortcutState>,
        installed: Box<ShortcutState>,
    },
    Service {
        name: String,
        previous: Box<ServiceState>,
        installed: Box<ServiceState>,
    },
    Protocol {
        scope: SelectedScope,
        scheme: String,
        previous: ProtocolState,
        installed: ProtocolState,
    },
    ProgId {
        scope: SelectedScope,
        id: String,
        previous: ProgIdState,
        installed: ProgIdState,
    },
    Extension {
        scope: SelectedScope,
        extension: String,
        previous: ExtensionState,
        installed: ExtensionState,
    },
    UninstallEntry {
        scope: SelectedScope,
        key_path: String,
        previous: Option<UninstallEntryState>,
        installed: Option<UninstallEntryState>,
    },
    /// Generic marker for unsupported or opaque nodes in tests.
    Opaque,
}

/// Platform/runtime implementation that can actually perform an operation.
pub trait OperationExecutor {
    type Error;

    /// Perform the operation's side effect and return a durable receipt.
    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error>;

    /// Undo a previously applied operation using its durable receipt.
    fn rollback(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error>;

    /// Determine the real-world outcome of a node left `Running` after a crash.
    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error>;
}

/// Runtime-neutral cancellation probe.
pub trait CancellationProbe {
    fn is_cancelled(&self) -> bool;
}

/// Never cancelled (default).
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancel;

impl CancellationProbe for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
}
