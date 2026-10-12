use std::path::Path;

use zup_acquire::{CachePolicy, HandoffMode};
use zup_core::SelectedScope;

use crate::acquire;

pub struct Accepted {
    pub acquired: acquire::Acquired,
    pub session: zup_acquire::SessionSummary,
    pub scope: SelectedScope,
    pub mode: HandoffMode,
}

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
    zup_windows::AcquiredBundle::open(executable, &cache, &verified)
        .map_err(|error| Rejection::Unverified(error.to_string()))?;

    let resolved = zup_update::ResolvedRelease {
        descriptor: verified.release.clone(),
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
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}
