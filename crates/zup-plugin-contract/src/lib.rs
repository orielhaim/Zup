#![deny(unsafe_op_in_unsafe_fn)]

//! Host-side validation of a Zup plugin.
//!
//! Everything here runs on the machine that installs: the engine a component is
//! compiled and loaded with, the shape it has to have before it is allowed to
//! run, and the limits it runs inside. A plugin guest depends on none of it, so
//! a guest that wants the contract does not pull a WebAssembly runtime into its
//! own build.
//!
//! The contract itself - the WIT, its digest, and the limits - is
//! `zup-plugin-abi`, which both sides depend on and neither side owns.
//!
//! This crate is not an authoring surface. A plugin author depends on
//! `zup-sdk`; a host reaches the engine through `zup-plugin-runtime`.

mod bindings;
mod config;
mod fingerprint;
mod runtime;
mod validation;

pub use bindings::exports::zup::plugin::planner::{
    Context, FileAssociation, GeneratedFile, InstallScope, InstallationPlan, Launcher,
    LauncherLocation, PathEntry, PluginError, Protocol, ResourceItem, Service, ServiceStart,
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

/// The target this host was compiled for, which is the only target whose
/// precompiled plugins it will load.
pub const HOST_TARGET: &str = env!("ZUP_BUILD_TARGET");

/// The interface a plugin implements, named as diagnostics name it.
pub use zup_plugin_abi::{PLAN_FUNCTION_NAME as PLANNER_FUNCTION, WORLD};