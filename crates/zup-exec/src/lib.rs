//! Desired-versus-observed execution delta planning for zup.
//!
//! ```text
//! TargetPlan + HostSnapshot → ExecutionPlan
//! ```
//!
//! Pure and platform-API independent. A platform runtime reports observed
//! facts; this crate decides Create / Replace / NoOp / Conflict.

#![forbid(unsafe_code)]

mod ledger;
mod lifecycle;
mod observe;
mod operation;
mod plan;

pub use ledger::{
    ExtensionState, FileAssociationState, INSTALL_LEDGER_SCHEMA, InstallLedger, LauncherState,
    OwnedResource, ProtocolState, ServiceState,
};
pub use lifecycle::{LifecycleAction, LifecycleError, plan_lifecycle};
pub use observe::{
    HostSnapshot, ObservedExtensionState, ObservedFile, ObservedFileAssociation,
    ObservedFileAssociationState, ObservedFileState, ObservedLauncher, ObservedLauncherState,
    ObservedPathEntry, ObservedProtocol, ObservedProtocolState, ObservedService,
    ObservedServiceState, SearchPath, ServiceRuntimeState,
};
pub use operation::{
    Conflict, Delta, ExecutionPlan, ExecutionSummary, FileAssociationOperation,
    FileAssociationOperationKind, FileOperation, FileOperationKind, FilePrecondition,
    LauncherOperation, LauncherOperationKind, ManagedOperation, PathOperation, PathOperationKind,
    ProtocolOperation, ProtocolOperationKind, RemovalKind, RemovalOperation, ServiceOperation,
    ServiceOperationKind,
};
pub use plan::{ExecutionPlanError, plan_execution};
