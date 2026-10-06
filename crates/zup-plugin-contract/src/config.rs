//! The engine a plugin is compiled and loaded with.
//!
//! Every limit and version the host enforces is re-exported from
//! `zup-plugin-abi` rather than restated here. They are re-exported so callers
//! keep one path to name them, but there is one definition: a limit a guest is
//! held to and a limit the host enforces have to be the same number, and two
//! copies of a constant are two numbers the day one of them is edited.

use thiserror::Error;
use wasmtime::{Config, Engine, ProfilingStrategy, WasmBacktraceDetails, WasmFeatures};

pub use zup_plugin_abi::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_AOT_BYTES,
    MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, WASM_PAGE_BYTES,
    WASMTIME_VERSION,
};

/// Why an engine could not be created for a target.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid Wasmtime target {target:?}: {message}")]
    InvalidTarget { target: String, message: String },
    #[error("could not create the plugin engine: {message}")]
    Creation { message: String },
}

/// A configured engine, and the fingerprint that says what configuration it is.
///
/// The fingerprint is what an ahead-of-time artifact records, and what stops a
/// host from loading output a differently configured engine produced: a
/// precompiled component is only loadable by the exact engine that made it.
#[derive(Clone, Debug)]
pub struct PluginEngine {
    pub(crate) engine: Engine,
    fingerprint: crate::EngineFingerprint,
    pub(crate) target: String,
}

impl PluginEngine {
    /// An engine that compiles and runs for `target`.
    pub fn new(target: &str) -> Result<Self, EngineError> {
        let config = engine_config(target)?;
        let engine = Engine::new(&config).map_err(|error| EngineError::Creation {
            message: error.to_string(),
        })?;
        let fingerprint = crate::engine_fingerprint(target);
        Ok(Self {
            engine,
            fingerprint,
            target: target.to_owned(),
        })
    }

    /// An engine for the target this host was compiled for.
    pub fn host() -> Result<Self, EngineError> {
        Self::new(crate::HOST_TARGET)
    }

    /// What configuration this engine is.
    pub fn fingerprint(&self) -> crate::EngineFingerprint {
        self.fingerprint
    }

    /// The target this engine compiles and runs for.
    pub fn target(&self) -> &str {
        &self.target
    }
}

/// An engine configured the way a plugin is allowed to run.
///
/// Deterministic and non-concurrent features only: a guest that could observe a
/// floating-point result that differed between hosts, or spawn a thread, could
/// produce a plan whose content depended on the machine that produced it, and
/// the host validates plans by content.
pub(crate) fn engine_config(target: &str) -> Result<Config, EngineError> {
    let mut config = Config::new();
    config
        .target(target)
        .map_err(|error| EngineError::InvalidTarget {
            target: target.to_owned(),
            message: error.to_string(),
        })?;
    config.wasm_features(engine_features(), true);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    config.max_wasm_stack(MAX_WASM_STACK_BYTES);
    config.wasm_backtrace_details(WasmBacktraceDetails::Disable);
    config.debug_info(false);
    config.debug_symbols(false);
    config.profiler(ProfilingStrategy::None);
    Ok(config)
}

pub(crate) fn engine_features() -> WasmFeatures {
    let mut features = WasmFeatures::empty();
    for feature in [
        WasmFeatures::MUTABLE_GLOBAL,
        WasmFeatures::SIGN_EXTENSION,
        WasmFeatures::REFERENCE_TYPES,
        WasmFeatures::MULTI_VALUE,
        WasmFeatures::BULK_MEMORY,
        WasmFeatures::COMPONENT_MODEL,
    ] {
        features.insert(feature);
    }
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_target() {
        let error = PluginEngine::new("not a target triple").unwrap_err();
        assert!(matches!(error, EngineError::InvalidTarget { .. }));
    }

    /// A plan's content has to depend on the context and nothing else, so the
    /// engine may not be a source of variation. Every feature that would make a
    /// result depend on the host, or let a guest run concurrently, is off.
    #[test]
    fn disables_non_deterministic_and_concurrent_features() {
        let features = engine_features();
        assert!(!features.floats());
        assert!(!features.simd());
        assert!(!features.relaxed_simd());
        assert!(!features.threads());
        assert!(!features.shared_everything_threads());
        assert!(!features.cm_threading());
        assert!(!features.cm_async());
        assert!(!features.cm_async_stackful());
        assert!(!features.stack_switching());
    }
}
