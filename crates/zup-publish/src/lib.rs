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

pub use zup_core::format_bytes;
