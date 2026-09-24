#![deny(unsafe_op_in_unsafe_fn)]

mod bindings;
mod config;
mod fingerprint;
mod runtime;
mod validation;

pub const HOST_TARGET: &str = env!("ZUP_BUILD_TARGET");

pub use bindings::exports::zup::plugin::planner::{
    Context, FileType, GeneratedFile, InstallScope, InstallationPlan, PathEntry, PluginError,
    Protocol, ResourceItem, Service, ServiceStart, Shortcut, ShortcutLocation,
};
pub use config::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, EngineError, INVOCATION_DEADLINE_MILLIS,
    MAX_AOT_BYTES, MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES,
    MAX_MEMORY_COUNT, MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, PluginEngine, WASM_PAGE_BYTES,
    WASMTIME_VERSION,
};
pub use fingerprint::{EngineFingerprint, engine_fingerprint, wit_package_digest};
pub use runtime::InvocationError;
pub use validation::{ContractError, PLAN_FUNCTION_NAME, PLANNER_EXPORT_NAME, ValidatedComponent};
