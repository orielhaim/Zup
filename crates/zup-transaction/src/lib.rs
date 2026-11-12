#![forbid(unsafe_code)]

mod collection;
pub use collection::{
    CollectionFence, CollectionState, GenerationCollection, GenerationCollector,
    collect_generation, verify_generation_collection,
};
mod cleanup;
pub use cleanup::{
    authenticate_cleanup, cleanup_candidates, collected_deployments, validate_retention,
};
mod coordinator;
mod executor;
mod generation;
mod history;
pub use history::{HistoryFilesystem, HistoryStorage, StateNamespace};
mod materialization;
pub use generation::{
    GenerationAccess, GenerationReceipt, MaterializationObservation, MaterializationRecord,
    MaterializationState, PreparationCollection, PreparationTree, RetiredPreparation,
};
pub use materialization::{GenerationFilesystem, GenerationStorage, PreparationEntry};
pub mod fault_injection;
mod ledger;
mod lock;
pub use ledger::{
    OwnershipProjectionError, ledger_path, prepare_installation_record, project_committed_files,
    retire_resources, retired_owned_keys, valid_file_key, validate_installation_record,
};
mod ownership;
pub use ownership::{OwnershipError, validate_ownership};
mod publication;
pub use publication::{
    InstallationPublisher, PublicationObservation, PublicationState, observe_publication,
    verify_installation_evidence, verify_published_installation,
};
mod native_service;
mod reconciliation;
mod retained_content;
pub use retained_content::{RetainedFileSource, retained_file_sources};
mod record;
mod reservation;
pub use native_service::NativeServiceReceipt;
pub use reconciliation::{
    ReconciliationError, ReconciliationState, reconcile_committed, settled_records,
};
pub use reservation::InstallationReservation;
mod service;
pub use service::{
    NativeServiceManager, ServiceControlAuthority, ServiceControlCompletion, ServiceControlReceipt,
    ServiceObservation, apply_service_control, prepare_service_compensation,
    reconcile_service_control, reconcile_service_rollback, rollback_service_control,
    verify_service_control,
};
mod journal_creation;
mod storage;
pub use journal_creation::{
    JournalDirectory, JournalIdentity, JournalLease, LockedJournal, create_journal, journal_lock,
};

pub use coordinator::{
    OpenTransaction, SettledTransaction, TransactionCoordinator, TransactionError,
    TransactionOutcome, recover,
};
pub use executor::{
    CancellationProbe, NativeCacheCompletion, OperationExecutor, OperationReceipt, ReconcileResult,
    RollbackReconcileResult,
};
pub use lock::{LockError, LockScope, ResourceLock};
pub use record::{
    CorruptReason, JOURNAL_SCHEMA, NodeState, NodeStateError, PhaseError, RecoveryPhase,
    StoreError, TransactionFailure, TransactionPhase, TransactionRecord,
};
pub use storage::{CONTENT_STORE_DIRECTORY, ContentStoreIdentity};
pub use storage::{FilesystemTransactionStore, JournalVolume, TransactionStore};
pub use storage::{
    MAINTENANCE_PACKAGE_NAME, MAINTENANCE_RUNTIME_DIRECTORY, STATE_FOLDER, is_maintenance_path,
    scope_name,
};
mod persistent;
pub use persistent::{
    PersistentAccessState, PersistentCreation, PersistentCreationToken, PersistentDirectoryReceipt,
    PersistentFilesystem, PersistentPreparation, PersistentStorage,
};

pub use executor::{CleanupCompletion, CleanupDeferral};

mod resolution;
pub use resolution::{
    RecoveryAssessment, RecoveryBlocker, RecoveryCause, RecoveryDecision, RecoveryRequest,
    assess_recovery, recover_requested,
};
