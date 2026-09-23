//! Desired-versus-observed execution delta planning for zup.
//!
//! ```text
//! TargetPlan + MachineSnapshot → ExecutionPlan
//! ```
//!
//! Pure and platform-API independent. `zup-windows` reports observed facts;
//! this crate decides Create / Replace / NoOp / Conflict / RunOpaque.

#![forbid(unsafe_code)]

mod ledger;
mod lifecycle;
mod observe;
mod operation;
mod plan;

pub use ledger::{
    ExtensionState, INSTALL_LEDGER_SCHEMA, InstallLedger, OwnedResource, ProgIdState,
    ProtocolState, ServiceState, ShortcutState, UninstallEntryState, UninstallEntryValue,
};
pub use lifecycle::{LifecycleAction, LifecycleError, plan_lifecycle};
pub use observe::{
    MachineSnapshot, ObservedExtensionState, ObservedFile, ObservedFileState, ObservedFileType,
    ObservedPathEntry, ObservedProgIdState, ObservedProtocol, ObservedProtocolState,
    ObservedService, ObservedServiceState, ObservedShortcut, ObservedShortcutState, PathEntryState,
    ServiceRuntimeState,
};
pub use operation::{
    Conflict, Delta, ExecutionPlan, ExecutionSummary, ExternalActionOperation, FileOperation,
    FileOperationKind, FilePrecondition, FileTypeOperation, FileTypeOperationKind,
    ManagedOperation, PathOperation, PathOperationKind, ProtocolOperation, ProtocolOperationKind,
    RemovalKind, RemovalOperation, ServiceOperation, ServiceOperationKind, ShortcutOperation,
    ShortcutOperationKind, UninstallEntryOperation,
};
pub use plan::{ExecutionPlanError, normalize_path_entry, path_contains_entry, plan_execution};
