//! The limits a plugin guest is held to, and the versions its artifact carries.
//!
//! Every one of these is part of the contract rather than of an implementation:
//! a guest that exceeds them is refused, and an artifact whose versions do not
//! match is refused, so both sides have to agree on the numbers. They live here
//! so that the host, the build, and the guest SDK read the same values rather
//! than three copies of them.

/// The plugin ABI version an artifact declares.
///
/// Distinct from this crate's version: a documentation fix or a new helper does
/// not change the interface a plugin implements, and neither does a change to
/// Zup itself.
pub const PLUGIN_API_VERSION: &str = "1.0.0";

/// The WIT package version, matching the `package` line in the WIT.
pub const WIT_PACKAGE_VERSION: &str = "1.0.0";

/// The Wasmtime version whose ahead-of-time output the host will load.
///
/// A precompiled component is only loadable by the exact engine configuration
/// that produced it, so this is recorded in every artifact and checked before
/// a deserializer is handed a byte of one.
pub const WASMTIME_VERSION: &str = "49.0.0";

/// The layout version of a precompiled plugin blob.
pub const AOT_FORMAT_VERSION: u32 = 1;

/// Fuel granted to one guest call.
pub const MAX_FUEL_PER_CALL: u64 = 100_000_000;

/// Epoch-interruption ticks after which a running guest is interrupted.
pub const EPOCH_DEADLINE_TICKS: u64 = 1;

/// Wall-clock budget for one guest call, in milliseconds.
pub const INVOCATION_DEADLINE_MILLIS: u64 = 250;

/// How many times a guest may call back into the host during one invocation.
///
/// Zero: a plugin plans, it does not observe. Everything it needs to know
/// arrives in the planning context, so there is nothing for a host call to
/// return that the guest did not already have.
pub const MAX_HOST_CALLS: u64 = 0;

/// A WebAssembly page, in bytes.
pub const WASM_PAGE_BYTES: u64 = 64 * 1024;

/// The largest guest linear memory, in pages.
pub const MAX_MEMORY_PAGES: u64 = 512;

/// The largest guest linear memory, in bytes.
pub const MAX_MEMORY_BYTES: usize = MAX_MEMORY_PAGES as usize * WASM_PAGE_BYTES as usize;

/// How many memories a guest may declare.
pub const MAX_MEMORY_COUNT: usize = 4;

/// The largest table a guest may declare, in elements.
pub const MAX_TABLE_ELEMENTS: u64 = 10_000;

/// How many tables a guest may declare.
pub const MAX_TABLE_COUNT: usize = 4;

/// How many component instances may exist at once.
pub const MAX_INSTANCES: usize = 8;

/// The most resources one plan may declare.
pub const MAX_PLAN_RESOURCES: usize = 4_096;

/// The most bytes one plan or refusal may occupy.
pub const MAX_PLAN_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// The largest ahead-of-time blob the host will load, in bytes.
pub const MAX_AOT_BYTES: usize = 64 * 1024 * 1024;

/// The largest guest call stack, in bytes.
pub const MAX_WASM_STACK_BYTES: usize = 1024 * 1024;