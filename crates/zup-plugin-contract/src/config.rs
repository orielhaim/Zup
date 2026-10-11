use thiserror::Error;
use wasmtime::{Config, Engine, ProfilingStrategy, WasmBacktraceDetails, WasmFeatures};

pub use zup_plugin_abi::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_AOT_BYTES,
    MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, WASM_PAGE_BYTES,
    WASMTIME_VERSION,
};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid Wasmtime target {target:?}: {message}")]
    InvalidTarget { target: String, message: String },
    #[error("could not create the plugin engine: {message}")]
    Creation { message: String },
}

#[derive(Clone, Debug)]
pub struct PluginEngine {
    pub(crate) engine: Engine,
    fingerprint: crate::EngineFingerprint,
    pub(crate) target: String,
}

impl PluginEngine {
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

    pub fn host() -> Result<Self, EngineError> {
        Self::new(crate::HOST_TARGET)
    }

    pub fn fingerprint(&self) -> crate::EngineFingerprint {
        self.fingerprint
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}

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
