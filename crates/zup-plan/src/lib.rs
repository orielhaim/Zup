#![forbid(unsafe_code)]

mod error;
mod plan_types;
mod planner;
mod plugins;
mod request;
mod resolve;
mod resources;
mod select;

pub use error::PlanError;
pub use plan_types::{DesiredState, PlanSummary};
pub use planner::plan;
pub use plugins::{
    CancellationQuery, GeneratedFile, MAX_PLUGIN_ARGUMENT_BYTES, MAX_PLUGIN_ARGUMENTS,
    MAX_PLUGIN_ERROR_BYTES, MAX_PLUGIN_GENERATED_BYTES, MAX_PLUGIN_GENERATED_FILE_BYTES,
    MAX_PLUGIN_RESOURCES, MAX_PLUGIN_STRING_BYTES, MAX_PLUGIN_TEXT_BYTES, NeverCancelled,
    PlannedInstallation, PluginExecutor, PluginFailure, PluginPlanningContext, PluginResource,
    PluginResourceProposal, sanitize_error,
};
pub use request::{ComponentOverrides, InstallDirectory, PlanRequest};
pub use resolve::{resolve_install_directory, resolve_template};
pub use resources::{
    DesiredFile, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, PlannedPrerequisite,
};
pub use select::select_components;

mod command;
mod delta;
mod files;
mod install_locations;
mod inventory;
mod lifecycle;
mod observe;
mod operation;
mod paths;
mod target_path;
mod target_support;

pub use command::CommandSpec;
pub use delta::{ExecutionPlanError, plan_execution};
pub use files::resolve_files;
pub use install_locations::{InstallLocationError, InstallLocationResolver};
pub use inventory::{
    ExtensionState, FileAssociationState, INSTALL_LEDGER_SCHEMA, InstallLedger, LauncherState,
    OwnedResource, ProtocolState, ServiceRegistrationChange, ServiceState, ServiceUpdate,
    owned_content_digests,
};
pub use lifecycle::{DeploymentIntent, LifecycleAction, LifecycleError, plan_lifecycle};
pub use observe::{
    HostSnapshot, ObservedExtensionState, ObservedFile, ObservedFileAssociation,
    ObservedFileAssociationState, ObservedFileState, ObservedLauncher, ObservedLauncherState,
    ObservedPathEntry, ObservedProtocol, ObservedProtocolState, ObservedService,
    ObservedServiceState, SearchPath, ServiceRuntimeState,
};
pub use operation::{
    Conflict, ExecutionPlan, FileAssociationOperation, FileOperation, FileOperationKind,
    FilePrecondition, IntegrationChangeKind, LauncherOperation, PathOperation, PathOperationKind,
    ProtocolOperation, RemovalKind, RemovalOperation, ServiceOperation,
};
pub use paths::{TemplateResolveError, resolve_template_path};
pub use target_path::{TargetPath, TargetPathError};
pub use target_support::{
    ArtifactSupport, Elevation, ScopeSupport, TargetCapabilities, capabilities,
};

mod validation;
pub use validation::ContractError;

mod binding;
pub use binding::{BindingMismatch, DeploymentBinding};

mod service_graph;
