//! Transaction graph, durable record, and recovery engine for zup.
//!
//! ```text
//! TransactionInput → TransactionPlan → TransactionRecord → TransactionCoordinator
//! ```
//!
//! Synchronous and deterministic. No Windows APIs, no async runtime.

#![forbid(unsafe_code)]

mod content_store;
mod coordinator;
mod executor;
mod id;
mod input;
mod installed;
mod lock;
mod plan;
mod record;
mod store;

pub use content_store::{CONTENT_STORE_DIRECTORY, ContentStoreIdentity, is_store_shape};
pub use coordinator::{TransactionCoordinator, TransactionError, TransactionOutcome, recover};
pub use executor::{CancellationProbe, OperationExecutor, OperationReceipt, ReconcileResult};
pub use id::{OperationId, TransactionId};
pub use input::{
    BackendOperation, BackendOperationIntent, FileDelta, FilePrecondition, FileRemoval,
    FileRemovalKind, FileWork, MAX_BACKEND_DEPENDENCIES, MAX_BACKEND_PAYLOAD_BYTES,
    TransactionInput, TransactionInputError, TransactionResource,
};
pub use installed::{
    MAINTENANCE_INDEX_NAME, MAINTENANCE_PACKAGE_NAME, MAINTENANCE_RUNTIME_DIRECTORY, STATE_FOLDER,
    is_maintenance_path, maintenance_directory, maintenance_root, maintenance_runtime_path,
    scope_name,
};
pub use lock::{InstallationLock, LockError, LockScope};
pub use plan::{
    Dependency, NodeKind, NodeMeta, Phase, TRANSACTION_PLAN_SCHEMA, TransactionAudit,
    TransactionNode, TransactionPlan, TransactionPlanError, compile_transaction,
};
pub use record::{
    CorruptReason, JOURNAL_SCHEMA, NodeState, NodeStateError, PhaseError, StoreError,
    TransactionPhase, TransactionRecord,
};
pub use store::{FilesystemTransactionStore, TransactionStore};
