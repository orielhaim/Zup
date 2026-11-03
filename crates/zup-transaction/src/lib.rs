#![forbid(unsafe_code)]

mod coordinator;
mod executor;
mod id;
mod input;
mod ledger;
mod lock;
pub use ledger::{
    LedgerProjectionError, ledger_path, project_committed_files, retire_resources,
    retired_ledger_keys, valid_file_key, validate_ledger_identity,
};
mod ownership;
pub use ownership::{OwnershipError, validate_ownership};
mod plan;
mod private;
mod reconciliation;
mod record;
pub use reconciliation::{
    LedgerPublication, PublicationError, ReconciliationError, ReconciliationState,
    ledger_publication, reconcile_committed, retired_records, settled_records,
};
mod storage;

pub use coordinator::{
    OpenTransaction, SettledTransaction, TransactionCoordinator, TransactionError,
    TransactionOutcome, recover,
};
pub use executor::{CancellationProbe, OperationExecutor, OperationReceipt, ReconcileResult};
pub use id::{OperationId, TransactionId};
pub use input::{
    BackendOperation, BackendOperationIntent, FileRemoval, MAX_BACKEND_DEPENDENCIES,
    MAX_BACKEND_PAYLOAD_BYTES, TransactionInput, TransactionInputError, TransactionResource,
};
pub use lock::{InstallationLock, LockError, LockScope};
pub use plan::{
    Dependency, NodeKind, NodeMeta, Phase, TRANSACTION_PLAN_SCHEMA, TransactionAudit,
    TransactionNode, TransactionPlan, TransactionPlanError, compile_transaction,
};
pub use private::{
    absent_collision, ensure_dir as ensure_private_dir, publish_full, publish_private, read_private,
};
pub use record::{
    CorruptReason, JOURNAL_SCHEMA, NodeState, NodeStateError, PhaseError, PublishError, StoreError,
    TransactionPhase, TransactionRecord, VisiblePublish, settle_visible,
};
pub use storage::{CONTENT_STORE_DIRECTORY, ContentStoreIdentity};
pub use storage::{FilesystemTransactionStore, JournalVolume, TransactionStore};
pub use storage::{
    MAINTENANCE_INDEX_NAME, MAINTENANCE_PACKAGE_NAME, MAINTENANCE_RUNTIME_DIRECTORY, STATE_FOLDER,
    is_maintenance_path, maintenance_directory, maintenance_root, maintenance_runtime_path,
    scope_name,
};
