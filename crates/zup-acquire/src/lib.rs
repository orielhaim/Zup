//! The reusable content acquisition engine for zup.
//!
//! An installer needs a **verified content closure**: the exact set of immutable
//! blobs a transaction requires before it may touch the machine. This crate
//! computes that closure, gets every blob into a verified local cache, and stops
//! at a barrier. It does not install anything, and it does not know what
//! transport the bytes came from.
//!
//! ```text
//! release graph (authenticated elsewhere)
//!   ↓  variant manifest + catalog
//! AcquisitionPlan          the exact closure, with reasons and an estimate
//!   ↓
//! AcquisitionSession       bounded concurrency, priorities, cancellation
//!   ↓  ArtifactSource × N   embedded · cache · directory · HTTP · OCI
//!   ↓
//! ContentCache             verified, content-addressed, resumable
//!   ↓
//! Barrier                  every required byte present and verified
//!   ↓
//! the existing zup transaction engine, unchanged
//! ```
//!
//! # Why the separation is load-bearing
//!
//! The same call serves a fresh install, an update, a modify, and a repair,
//! because none of them needs to know where a blob came from. The network is an
//! untrusted transport: what makes content trustworthy is that a release graph
//! authenticated `sha256:X, size:N` and the bytes hashed to `X` locally. An
//! offline artifact, a CDN, a USB stick, and a warm cache are therefore
//! interchangeable, and none of them is trusted for being reachable.
//!
//! # What this crate deliberately does not have
//!
//! No HTTP, no TUF, no platform APIs, and no installer knowledge. Those live in
//! `zup-acquire-http` and `zup-update` and are added at the edges, so the engine
//! can be tested against an in-memory source and a directory with no network in
//! the picture at all.
//!
//! # The barrier
//!
//! Nothing outside the cache is exposed while it is being filled, and
//! [`Barrier`] is the only thing that authorizes a machine mutation. A
//! transaction may create its journal, its quarantine, and its staging area
//! first; it may not publish an application file, a registry entry, a service,
//! or a PATH change until every required resource is verified. That is what
//! makes "download, then install" safe rather than merely sequential.

#![forbid(unsafe_code)]

mod cache;
mod cancellation;
mod descriptor;
mod error;
mod filesystem;
mod handoff;
mod host;
mod layout;
mod local;
mod plan;
mod progress;
mod release;
mod retention;
mod session;
mod source;

pub use cache::{
    BlobPaths, BlobReader, BlobWriter, CacheObject, CachePolicy, CacheProbe, ContentCache,
    RESERVATION_STALE, RESUME_RECORD_INTERVAL, RESUME_SCHEMA, ResumeRecord, VerifiedBlob, Verify,
};
pub use cancellation::{CancelFlag, Cancellation, NeverCancelled, cancellable_sleep};
pub use descriptor::{
    ContentCompression, ContentDescriptor, ContentKind, ContentPriority, ContentReason,
    MAX_CATALOG_BYTES, MAX_CLOSURE_ITEMS, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, MAX_RUNTIME_BYTES,
    RelativeContentPath, default_priority, schedule_order,
};
pub use error::{AcquireError, CacheError, SourceError, failing_digest};
pub use filesystem::{CacheFileSystem, PortableCacheFileSystem};
pub use handoff::{
    HANDOFF_SCHEMA, HandoffError, HandoffMode, MAX_HANDOFF_BYTES, RuntimeHandoff, SessionSummary,
};
pub use host::{
    HostArchitecture, HostProfile, Incompatible, SelectionError, check_variant, select_variant,
};
pub use layout::{
    BLOB_ROOT, CATALOG_SCHEMA, CatalogEntry, ContentCatalog, METADATA_ROOT, RELEASE_ROOT,
    WebLayout, blob_path, catalog_path, check_channel, check_segment, release_path,
    release_version_path, variant_manifest_path,
};
pub use local::{DirectorySource, MemorySource, SatisfiedSource};
pub use plan::{AcquisitionEstimate, AcquisitionItem, AcquisitionPlan, format_bytes};
pub use progress::{AcquisitionEvent, AcquisitionPhase, AcquisitionProgress};
pub use release::{
    DocumentRef, OnlineTrust, RELEASE_SCHEMA, ReleaseDescriptor, ReleaseDownload,
    ReleaseDownloadKind, ReleasePin, ReleaseRequirements, ReleaseVariant,
};
pub use retention::{
    DEFAULT_GRACE, MAX_RETENTION_BYTES, RETENTION_FILE, RETENTION_SCHEMA, RetentionReport,
    RetentionState, read_retention, retention_path, sweep, sweep_root, write_retention,
};
pub use session::{
    AcquisitionOutcome, AcquisitionSession, Barrier, BlobStager, NoStaging, ProgressSink,
    RETRY_MARKER, SchedulerConfig, SharedCancellation, StagerFuture, retry_detail,
};
pub use source::{AcquireRequest, ArtifactSource, SourceChain, SourceFuture};

/// The identity an installed machine persists about the release that produced
/// it.
///
/// Re-exported rather than moved so a caller that is holding a release graph
/// does not have to know that the ownership ledger stores the same type.
pub use zup_core::ReleaseIdentity;
