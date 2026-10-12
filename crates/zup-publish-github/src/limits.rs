use zup_publish::HostLimits;

pub const MAX_ASSETS: usize = 1000;

pub const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub const LIMITS: HostLimits = HostLimits {
    max_assets: MAX_ASSETS,
    max_asset_bytes: MAX_ASSET_BYTES,
};

pub fn accepts(size: u64) -> bool {
    size < MAX_ASSET_BYTES
}

pub const PACKAGE_SHARD_BYTES: u64 = 1536 * 1024 * 1024;

pub fn limit_text() -> String {
    format!("{} MiB", MAX_ASSET_BYTES / (1024 * 1024))
}
