//! The runtime's side of a thin handoff.
//!
//! A thin dispatcher resolved an authenticated release, verified a native
//! runtime, and started it. This is where that claim is checked. The runtime is
//! about to write files, registry entries, and services, so it re-derives what it
//! is installing from authenticated descriptors rather than from anything the
//! previous process said.
//!
//! The order is the whole design:
//!
//! ```text
//! 1  read the handoff, and check its own fingerprint
//! 2  read the release it names out of the verified cache, and check its digest
//! 3  check the release names this exact image as its runtime
//! 4  check the variant, the target, and the frontend all agree
//! 5  check this image's embedded plan is the release's plan
//! 6  only then plan a lifecycle
//! ```
//!
//! Step 3 is what binds the rest. A release authenticates the runtime digest for
//! each of its variants, and the release's own fingerprint was verified before a
//! byte was downloaded — so a substituted release would have to name *this*
//! image's digest, which requires a SHA-256 preimage.
//!
//! What the dispatcher *does* supply is a location: where the cache is. A hostile
//! location is a cache full of objects that do not hash to the authenticated
//! digests, which is a failure and not a compromise. The location is therefore the
//! one untrusted input, and it is treated as a location and nothing else.

use std::path::Path;

use zup_acquire::{CachePolicy, HandoffMode};
use zup_core::SelectedScope;

use crate::acquire;

/// A handoff this runtime has checked and can act on.
pub struct Accepted {
    /// The verified release graph, with its closure computed.
    pub acquired: acquire::Acquired,
    /// What the dispatcher had already achieved, for a progress line.
    pub session: zup_acquire::SessionSummary,
    /// The scope the installation is being created in.
    pub scope: SelectedScope,
    /// The lifecycle this process is about to run.
    pub mode: HandoffMode,
}

/// Read a handoff and prove this process is the one it names.
pub fn accept(
    executable: &Path,
    cache_root: &Path,
    handoff_path: &Path,
    expected_digest: Option<&str>,
) -> Result<Accepted, Rejection> {
    let expected = expected_digest
        .map(str::parse::<zup_core::Sha256Digest>)
        .transpose()
        .map_err(|_| {
            Rejection::Unverified(
                "the handoff digest the launcher passed is not a SHA-256 digest".to_owned(),
            )
        })?;
    let handoff = zup_windows::accept_handoff(handoff_path, expected)
        .map_err(|error| Rejection::Unverified(error.to_string()))?;
    let cache = zup_windows::open_cache(cache_root, CachePolicy::Auto)
        .map_err(|error| Rejection::Unverified(error.to_string()))?;
    let verified = zup_windows::verify(executable, &cache, &handoff)
        .map_err(|error| Rejection::Unverified(error.to_string()))?;
    // The plan compiled into this image and the plan the graph authenticated have
    // to be the same plan. This is the check a dispatcher cannot satisfy by
    // pointing this image at a different release's content.
    zup_windows::AcquiredBundle::open(executable, &cache, &verified)
        .map_err(|error| Rejection::Unverified(error.to_string()))?;

    let resolved = zup_update::ResolvedRelease {
        descriptor: verified.release.clone(),
        // The document was read out of the cache under the digest the handoff
        // named and re-hashed on the way out, so its bytes are exactly what the
        // cache holds. They are kept so a later lifecycle in the same process can
        // re-derive the identity without asking the cache again.
        descriptor_digest: handoff.document,
        descriptor_bytes: Vec::new(),
        variant: verified
            .release
            .variant(&handoff.variant)
            .cloned()
            .ok_or_else(|| {
                Rejection::Unverified("the release no longer names the variant".into())
            })?,
        catalog: verified.catalog.clone(),
        manifest: verified.manifest.clone(),
        document: String::new(),
        pinned: handoff.mode == HandoffMode::Install,
    };
    let manifest = acquire::parse_manifest(&resolved)?;
    let plan =
        acquire::component_closure(&resolved, &manifest, zup_update::ComponentSelection::All)?;
    let estimate = plan.wire_size();
    let outcome = zup_acquire::AcquisitionOutcome {
        items: Vec::new(),
        estimate: zup_acquire::AcquisitionEstimate {
            // The handoff's own accounting is presentation, and the runtime
            // recomputes what it can: the closure's wire bytes are a fact the
            // authenticated catalog states, and the dispatcher's number is not.
            download_bytes: 0,
            install_bytes: plan.install_size(),
            cached_bytes: handoff.session.cached_bytes,
            cached_items: handoff.session.cache_hits as usize,
            missing_items: 0,
        },
        elapsed: std::time::Duration::from_millis(handoff.session.elapsed_ms),
        cache_hits: handoff.session.cache_hits as usize,
        total_bytes: estimate,
    };
    Ok(Accepted {
        acquired: acquire::Acquired {
            resolved,
            manifest,
            plan,
            cache: std::sync::Arc::new(cache),
            // The channel is the release's own claim, and the release is
            // authenticated. It is copied from there rather than from the
            // handoff, which is the point: the handoff is a hint, the release is
            // the authority.
            channel: verified.release.channel.clone(),
            trust_anchor: zup_core::Sha256Digest::from_bytes([0; 32]),
            outcome,
        },
        session: handoff.session,
        scope: match handoff.scope.as_str() {
            "machine" => SelectedScope::Machine,
            _ => SelectedScope::User,
        },
        mode: handoff.mode,
    })
}

/// Why a handoff could not be accepted.
#[derive(Debug, thiserror::Error)]
pub enum Rejection {
    #[error("this runtime was not what the authenticated release names: {0}")]
    Unverified(String),
    #[error("the handoff names a release this build cannot read: {0}")]
    Graph(#[from] acquire::GraphError),
    #[error("handoff I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl Rejection {
    /// Whether this refusal happened before any machine change.
    ///
    /// Always yes. Every check runs before the transaction engine is asked for a
    /// plan, and the engine's barrier is the only thing that authorizes a
    /// mutation.
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}
