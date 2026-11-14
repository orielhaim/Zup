#![forbid(unsafe_code)]

mod error;
mod generation;
mod history;
pub use history::{HistoryCleanup, HistoryFileTarget, HistoryObject, JournalMember, MetadataFile};
mod native_cache;
mod native_service;
pub use generation::{
    ActivationSelection, GenerationIntent, GenerationOperation, GenerationPreparation,
    GenerationPublication, GenerationSource, PlannedGeneration, validate_maintenance_content,
};
pub use native_cache::{NativeCacheKind, NativeCacheOperation};
pub use native_service::{NativeServiceOperation, ServiceActivation};
mod plan_types;
mod planner;
mod plugins;
mod rebinding;
mod request;
mod retained;
pub use retained::RetainedDeploymentError;
mod requirements;
mod resolve;
mod resources;
pub use requirements::{DeploymentRequirements, RequiredFile};
mod select;
mod space;

pub use error::PlanError;
pub use plan_types::{BoundDeployment, DesiredState, PlanSummary};
pub use planner::plan;
pub use plugins::{
    CancellationQuery, GeneratedFile, MAX_PLUGIN_ARGUMENT_BYTES, MAX_PLUGIN_ARGUMENTS,
    MAX_PLUGIN_ERROR_BYTES, MAX_PLUGIN_GENERATED_BYTES, MAX_PLUGIN_GENERATED_FILE_BYTES,
    MAX_PLUGIN_RESOURCES, MAX_PLUGIN_STRING_BYTES, MAX_PLUGIN_TEXT_BYTES, NeverCancelled,
    PlannedInstallation, PluginExecutor, PluginFailure, PluginPlanningContext, PluginResource,
    PluginResourceProposal, sanitize_error,
};
pub use request::{
    ComponentOverrides, DeploymentSelection, GenerationPolicy, GenerationSelection,
    InstallDirectory, PlanRequest,
};
pub use resolve::{resolve_install_directory, resolve_template};
pub use resources::{
    DesiredFile, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, PlannedPrerequisite,
};
pub use select::select_components;

mod delta;
mod files;
mod install_locations;
mod inventory;
mod lifecycle;
mod observe;
mod operation;
mod paths;
mod target_support;

pub use delta::{ExecutionPlanError, plan_execution};
pub use files::resolve_files;
pub use install_locations::{InstallLocationError, InstallLocationResolver};
pub use inventory::{
    DeploymentSnapshot, ExtensionState, FileAssociationState, INSTALLATION_RECORD_SCHEMA,
    InstallationCommit, InstallationRecord, InstallationRecordCodecError, InstallationRecordError,
    InstallationState, LauncherState, OwnedResource, OwnedService, ProtocolState,
    ServiceRegistrationChange, ServiceState, ServiceUpdate, owned_content_digests,
};
pub use lifecycle::{
    DeploymentIntent, DeploymentTransition, LifecycleAction, LifecycleError, plan_lifecycle,
};
pub use observe::{
    HostSnapshot, ObservedExtensionState, ObservedFile, ObservedFileAssociation,
    ObservedFileAssociationState, ObservedFileState, ObservedLauncher, ObservedLauncherState,
    ObservedPathEntry, ObservedProtocol, ObservedProtocolState, ObservedService,
    ObservedServiceState, SearchPath, ServiceRuntimeState,
};
pub use operation::{
    Conflict, ExecutionPlan, FileAssociationOperation, FileOperation, FileOperationKind,
    FilePrecondition, IntegrationChangeKind, LauncherOperation, PathBinding, PathOperation,
    PathOperationKind, PathPrecondition, PathPresence, ProtocolOperation, RemovalKind,
    RemovalOperation, ServiceCompensationError, ServiceOperation,
};
pub use paths::{TemplateResolveError, resolve_template_path};
pub use target_support::{
    ArtifactSupport, Elevation, ScopeSupport, TargetCapabilities, capabilities,
};
use zup_core::TargetPath;

mod validation;
pub use validation::ContractError;

mod binding;
pub use binding::{BindingMismatch, DeploymentBinding};

mod service_graph;

mod operation_id;
pub use operation_id::OperationId;
mod effect_input;
pub use effect_input::{
    BackendOperation, BackendOperationIntent, DeploymentEffects, DeploymentEffectsError,
    DeploymentResource, FileRemoval, MAX_BACKEND_DEPENDENCIES, MAX_BACKEND_PAYLOAD_BYTES,
};
mod graph;
pub use graph::{
    DEPLOYMENT_PLAN_SCHEMA, Dependency, DeploymentAudit, DeploymentNode, DeploymentPlan,
    DeploymentPlanError, NodeKind, NodeMeta, Phase, compile_deployment,
};
mod service_control;
pub use service_control::{
    ServiceControlIntent, ServiceControlOperation, ServiceControlStage, ServiceScopeObservation,
    ServiceTerminalState, WindowsJobState, bind_service_dependencies, disabled_startup,
    plan_service_controls, requires_startup_inhibition, validate_owned_service_inputs,
    validate_runtime_preservation, validate_startup_inhibition,
};
mod cleanup;
mod persistent;
mod retention;
pub use cleanup::{
    CleanupCandidate, CleanupOperation, FixedCleanup, GenerationCleanup, GenerationRetirement,
};
pub use persistent::{
    FilesystemDataBinding, OwnedPersistentDirectory, PersistentBinding, PersistentClaimSelection,
    PersistentDirectoryIntent, PersistentDirectoryOperation, PersistentDirectoryStage,
    PersistentOwnership, ServiceDataPrincipal, ServiceDataWriter, SystemdStateBinding,
    bind_service_arguments, service_state_name,
};
pub use retention::{RetentionChange, RetiredDeployment, RetiredDeploymentSelection};
