#![forbid(unsafe_code)]

//! The canonical Zup plugin ABI.
//!
//! A plugin is a WebAssembly component that exports one function, and this
//! crate is the definition of what that function is. It is deliberately the
//! smallest crate in the plugin system: WIT text, the digest that proves a
//! component was built against this exact contract, and the limits a guest is
//! held to. No engine, no linker, no host.
//!
//! # Why this crate exists
//!
//! A plugin author and the host that runs the plugin both need the WIT, and
//! neither of them needs the other. The author needs it to generate bindings;
//! the host needs it to validate a component and to prove that an ahead-of-time
//! artifact was produced against the same contract. Splitting those two
//! concerns is what lets a guest SDK depend on the contract without dragging
//! in a WebAssembly runtime, and lets a runtime validate a component without
//! owning the bindings generator.
//!
//! This crate also owns the WIT *file*. Both sides read it from here rather
//! than keeping a copy, so there is exactly one definition of the ABI and a
//! host cannot disagree with a guest about what it implements. A crate that
//! generates bindings from it reads this crate's build metadata for the path
//! rather than vendoring a second copy.

mod limits;

pub use limits::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_AOT_BYTES,
    MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, WASM_PAGE_BYTES, WASMTIME_VERSION,
    WIT_PACKAGE_VERSION,
};

use sha2::{Digest, Sha256};

/// The WIT contract, verbatim.
///
/// The one copy of the plugin interface definition in this repository. Both the
/// guest SDK's bindings and the host's validation are generated from it, so a
/// change here is a change to the ABI rather than a refactor of one side of it.
pub const WIT_PACKAGE: &str = include_str!("../wit/zup-plugin.wit");

/// The world a Zup plugin exports.
pub const WORLD: &str = "plugin";

/// The interface a Zup plugin implements, as the host names it in diagnostics.
pub const PLANNER_EXPORT_NAME: &str = "zup:plugin/planner@1.0.0";

/// The one function a Zup plugin exports.
pub const PLAN_FUNCTION_NAME: &str = "plan";

/// The digest of [`WIT_PACKAGE`].
///
/// Written into every compiled plugin artifact and checked when one is loaded,
/// so a component built against a different contract is refused rather than
/// called with a signature its author never wrote.
pub fn wit_package_digest() -> [u8; 32] {
    Sha256::digest(WIT_PACKAGE.as_bytes()).into()
}