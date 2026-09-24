use thiserror::Error;
use wasmtime::{Config, Engine, ProfilingStrategy, WasmBacktraceDetails, WasmFeatures};

pub const PLUGIN_API_VERSION: &str = "1.0.0";
pub const WASMTIME_VERSION: &str = "49.0.0";
pub const AOT_FORMAT_VERSION: u32 = 1;
pub const MAX_WASM_STACK_BYTES: usize = 1024 * 1024;
pub const MAX_FUEL_PER_CALL: u64 = 100_000_000;
pub const EPOCH_DEADLINE_TICKS: u64 = 1;
pub const INVOCATION_DEADLINE_MILLIS: u64 = 250;
pub const MAX_HOST_CALLS: u64 = 0;
pub const WASM_PAGE_BYTES: u64 = 64 * 1024;
pub const MAX_MEMORY_PAGES: u64 = 512;
pub const MAX_MEMORY_BYTES: usize = MAX_MEMORY_PAGES as usize * WASM_PAGE_BYTES as usize;
pub const MAX_MEMORY_COUNT: usize = 4;
pub const MAX_TABLE_ELEMENTS: u64 = 10_000;
pub const MAX_TABLE_COUNT: usize = 4;
pub const MAX_INSTANCES: usize = 8;
pub const MAX_PLAN_RESOURCES: usize = 4_096;
pub const MAX_PLAN_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_AOT_BYTES: usize = 64 * 1024 * 1024;

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
