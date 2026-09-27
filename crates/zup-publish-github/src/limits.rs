//! GitHub's own limits, as this provider needs them.
//!
//! Every number here is a documented GitHub limit, not a preference. They are
//! collected in one place because a limit that is checked somewhere other than
//! where it is stated is a limit nobody can reason about, and because the
//! interesting one — the per-asset ceiling — is the difference between "refuse
//! before creating a release" and "upload nine gigabytes and then fail".
//!
//! # The numbers, and what each one is for
//!
//! | limit                | value  | what happens past it                                    |
//! |----------------------|--------|---------------------------------------------------------|
//! | assets per release   | 1000   | the API refuses the upload; the release is unfixable    |
//! | bytes per asset      | 2 GiB  | the API refuses the upload                              |
//! | total release size   | none   | a release of 400 GiB is allowed and is nobody's problem  |
//! | bandwidth quota      | none   | throughput is throttled, not refused                    |
//!
//! The absence of the last two is the reason this provider can publish a whole
//! multi-architecture release in one go, and the reason it must not be asked to
//! do it twice.

use zup_publish::HostLimits;

/// Assets one GitHub release may carry.
pub const MAX_ASSETS: usize = 1000;

/// The largest single asset, exclusive: a file of exactly this size is refused.
///
/// Two gibibytes, which is where a 32-bit size field and a CDN upload limit
/// meet. Nothing else about a release is bounded, so this is the number a
/// publisher has to know before it starts.
pub const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// GitHub's limits, as the release plane needs them.
pub const LIMITS: HostLimits = HostLimits {
    max_assets: MAX_ASSETS,
    max_asset_bytes: MAX_ASSET_BYTES,
};

/// Whether a file fits in one GitHub asset.
pub fn accepts(size: u64) -> bool {
    size < MAX_ASSET_BYTES
}

/// The ceiling a transport package is packed to before it is split.
///
/// Comfortably under [`MAX_ASSET_BYTES`], because a package exactly at the limit
/// has no room for a host that rounds, re-encodes, or appends a trailer, and a
/// package that cannot be uploaded is worth less than a slightly less tidy
/// shard boundary.
pub const PACKAGE_SHARD_BYTES: u64 = 1536 * 1024 * 1024;

/// One past the largest asset, for a message.
pub fn limit_text() -> String {
    format!("{} MiB", MAX_ASSET_BYTES / (1024 * 1024))
}
