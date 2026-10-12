pub const PLUGIN_API_VERSION: &str = "1.0.0";

pub const WIT_PACKAGE_VERSION: &str = "1.0.0";

pub const WASMTIME_VERSION: &str = "49.0.0";

pub const AOT_FORMAT_VERSION: u32 = 1;

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

pub const MAX_WASM_STACK_BYTES: usize = 1024 * 1024;
