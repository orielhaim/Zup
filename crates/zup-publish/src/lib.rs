//! The release plane: what a release is, and what publishing it means.
//!
//! # Why this crate exists
//!
//! The artifact graph knows how to *build* things. A `TargetProfile` becomes a
//! `DistributionVariant`, variants compose into `DistributionArtifact`s, and a
//! build emits a release manifest naming what it produced. None of that answers
//! what a release *is*, because a release is a promise about files and their
//! identity, and the graph is a statement about bytes.
//!
//! So this crate introduces the layer between them:
//!
//! ```text
//! Build plane        TargetProfile → DistributionVariant → DistributionArtifact
//! Release plane       ReleasePlan → Publisher → provider release
//! Content plane       ContentSource → provider assets / static origin / future OCI
//! Trust plane         TUF, or whatever else a release is authenticated under
//! ```
//!
//! These stay separate. A [`ReleasePlan`](plan::ReleasePlan) describes *what is
//! supposed to be released*: which files, at which digests, under which tag, in
//! which roles. It does not know what a provider is, where a file is fetched
//! from, or who verifies it. Everything a provider would otherwise have to be
//! told lives in three small pieces instead of one large trait:
//!
//! - [`HostLimits`](plan::HostLimits) - what a host can hold.
//! - [`RemoteAsset`](decision::RemoteAsset) - what a host currently reports.
//! - [`classify`](decision::classify) - what to do about the difference.
//!
//! # What this crate refuses to know
//!
//! No release identifier, no API version, no credential, no draft flag, no
//! discussion category, no host name. A GitHub release id has no meaning here,
//! and a release plan that carried one would have to be rewritten the moment a
//! project moved to another forge. Provider state belongs in a provider receipt
//! - see [`PublishReceipt`](receipt::PublishReceipt) for the neutral shape and
//! `zup-publish-github` for the one that carries the ids.
//!
//! # Roles, and why a file is not a role
//!
//! A release contains installers, update artifacts, auxiliary assets, and
//! manifests. These are genuinely different claims and the plan keeps them
//! apart, because a framework updater needs to tell them apart: a Tauri build's
//! install artifact and update artifact are the same `.exe`, plus a
//! `latest.json` and a `.sig` beside it; an Electron macOS build's are
//! different files. A plan that had one list called "artifacts" could express
//! neither.
//!
//! That is also the acceptance test for this model: a future adapter contributes
//! its own auxiliary outputs, and the publisher uploads them without ever naming
//! the framework that produced them.

#![forbid(unsafe_code)]

mod decision;
mod naming;
mod plan;
mod receipt;
mod report;

pub use decision::{
    AssetAction, AssetConflict, AssetState, ConflictReason, RemoteAsset, classify,
    describe_conflict,
};
pub use naming::{
    ASSET_NAME_MAX, DOCUMENT_PREFIX, DOCUMENT_SUFFIX, DocumentKind, asset_name, check_asset_name,
    document_name, document_path, is_asset_name_byte, package_name, safe_segment, shard_index,
    shard_name, shard_suffix,
};
pub use plan::{
    Application, ContentOrigin, HostLimits, OriginKind, PLAN_SCHEMA, PlanError, ProductClass,
    ProductRole, ReleasePlan, ReleaseProduct, SourceClaim, TagIntent, TagPolicy,
};
pub use receipt::{
    Notice, PRODUCT_STATES, PUBLICATION_STATES, ProductState, PublicationState, PublishReceipt,
    PublishedProduct, RECEIPT_SCHEMA,
};
pub use report::{
    PhaseReport, PhaseStatus, PublishReport, REPORT_SCHEMA, ReportBuilder, StepReport, StepStatus,
};

/// Render a byte count the way every zup report does.
///
/// One implementation, because a number formatted two different ways in two
/// reports is a defect a reader notices and nobody can act on.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}
