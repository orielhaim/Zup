#![deny(unsafe_op_in_unsafe_fn)]

mod bindings;
mod config;
mod fingerprint;
mod runtime;
mod validation;
mod wasi;

pub use bindings::exports::zup::plugin::planner::{
    ChangedServiceState, Context, DataAccess, DataCompatibility, DataDirectory, DataLocation,
    DataPath, DataSchema, FileAssociation, FixedReplacement, GeneratedFile, InitialServiceState,
    InstallScope, InstallationPlan, Launcher, LauncherLocation, OpenrcFailurePolicy,
    OpenrcTimeouts, PathEntry, PluginError, Protocol, ResourceItem, Service, ServiceExecution,
    ServiceFailurePolicy, ServiceLifecycle, ServiceReadiness, ServiceStart, ServiceTimeouts,
    SystemdFailurePolicy, SystemdRestartCondition, SystemdTimeouts, SystemdUser,
    WindowsFailurePolicy, WindowsServiceAccount, WindowsTimeouts,
};
pub use config::{EngineError, PluginEngine};
pub use fingerprint::{EngineFingerprint, engine_fingerprint};
pub use runtime::InvocationError;
pub use validation::{ContractError, ValidatedComponent};
pub use zup_plugin_abi::{
    COMPONENT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS,
    MAX_COMPONENT_BYTES, MAX_FUEL_PER_CALL, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASI_RESOURCES, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION,
    WASM_PAGE_BYTES, WASMTIME_VERSION,
};

pub const HOST_TARGET: &str = env!("ZUP_BUILD_TARGET");

pub use zup_plugin_abi::{PLAN_FUNCTION_NAME, PLANNER_EXPORT_NAME, WORLD, wit_package_digest};
