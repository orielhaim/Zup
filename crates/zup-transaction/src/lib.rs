//! Transaction graph, durable journal, and recovery engine for zup.
//!
//! ```text
//! ExecutionPlan → TransactionPlan → TransactionRecord → TransactionCoordinator
//! ```
//!
//! Synchronous and deterministic. No Windows APIs, no async runtime.

#![forbid(unsafe_code)]

mod coordinator;
mod executor;
mod id;
mod journal_fs;
mod plan;
mod record;
mod rollback;
mod store;

pub use coordinator::{TransactionCoordinator, TransactionError, TransactionOutcome, recover};
pub use executor::{
    CancellationProbe, NeverCancel, OperationExecutor, OperationReceipt, ReconcileResult,
};
pub use id::OperationId;
pub use plan::{
    Dependency, ManagedResource, NodeKind, NodeMeta, Phase, TransactionAudit, TransactionNode,
    TransactionPlan, TransactionPlanError, compile_transaction,
};
pub use record::{
    CorruptReason, JOURNAL_SCHEMA, NodeState, NodeStateError, PhaseError, StoreError,
    TransactionPhase, TransactionRecord,
};
pub use rollback::{RollbackCapability, RollbackGuarantee};
pub use store::{FilesystemTransactionStore, TransactionStore};
pub use uuid::Uuid;

/// Runtime identity of one transaction execution attempt.
pub type TransactionId = id::TransactionId;
