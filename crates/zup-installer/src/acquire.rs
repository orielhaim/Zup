//! One release graph, four operations.
//!
//! A fresh install, an update, a modify, and a repair differ in exactly two
//! things: which component set is asked for, and which lifecycle verb runs
//! afterwards. They are otherwise the same call - resolve an authenticated
//! release, select the machine's variant, compute the closure, fill a verified
//! cache, cross the barrier, and hand a bound identity to the transaction engine.
//!
//! That is the whole architectural claim of this module, and it is why there is
//! no update downloader and no repair downloader. There is a graph and an
//! acquisition engine, and four operations are four arguments to it.
//!
//! # What each operation asks for
//!
//! | operation | closure |
//! | --- | --- |
//! | install | every component the user selected, on an empty machine |
//! | update | the new release's whole closure, minus what the verified cache already holds |
//! | modify | the committed selection plus whatever `--enable` added |
//! | repair | exactly the digests behind the resources the ledger says drifted |
//!
//! The update row is a byte count, not an estimate. A blob this machine already
//! downloaded is the same object the new release names, so it costs zero network
//! bytes; a complete installer is the whole release on every machine for every
//! update.
//!
//! # The barrier
//!
//! Everything here happens before the transaction engine is asked for a plan.
//! `GraphError::left_machine_unchanged` is `true` for every variant, and the
//! reason is structural rather than aspirational: this module has no way to
//! publish an application file, a registry entry, or a service.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use zup_acquire::{
    AcquisitionOutcome, AcquisitionPlan, AcquisitionSession, CachePolicy, ContentKind,
    ContentReason, HostProfile, NeverCancelled, ProgressSink, RetentionState, SessionSummary,
    write_retention,
};
use zup_artifact::VariantManifest;
use zup_core::Sha256Digest;
use zup_update::{
    ComponentSelection, ReleaseResolver, ResolvedRelease, TrustContext, default_scheduler,
};

/// Why a graph could not be used for the requested operation.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("the release graph could not be used: {0}")]
    Release(#[from] zup_update::UpdateError),
    #[error("the variant manifest is not one this runtime understands: {0}")]
    Manifest(String),
    #[error("acquisition: {0}")]
    Acquire(#[from] zup_acquire::AcquireError),
    #[error("graph I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl GraphError {
    /// Whether this failure left the machine untouched.
    ///
    /// Always yes: every one of these is raised before the transaction engine is
    /// asked for a plan, and the engine's own barrier is the only thing that
    /// authorizes a mutation.
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}

/// A satisfied closure: the verified cache, and everything the graph said about
/// it.
///
/// This is the barrier's payload. The machine may now be mutated, and only
/// because every byte the plan named is present, re-hashed, and proven.
pub struct Acquired {
    /// The authenticated release.
    pub resolved: ResolvedRelease,
    /// The variant manifest, parsed and checked against the release's claims.
    pub manifest: VariantManifest,
    /// The exact closure that was satisfied.
    pub plan: AcquisitionPlan,
    /// The verified cache the content lives in.
    pub cache: Arc<zup_acquire::ContentCache>,
    /// The channel this graph was resolved through.
    pub channel: String,
    /// The publisher's trust anchor.
    pub trust_anchor: Sha256Digest,
    /// The barrier's own record.
    pub outcome: AcquisitionOutcome,
}

impl Acquired {
    /// What the closure cost, for a frontend that is continuing a progress bar.
    pub fn summary(&self) -> SessionSummary {
        let estimate = self.estimate();
        SessionSummary {
            total_bytes: estimate
                .download_bytes
                .saturating_add(estimate.cached_bytes),
            cached_bytes: estimate.cached_bytes,
            cache_hits: estimate.cached_items as u64,
            downloaded_items: estimate.missing_items as u64,
            elapsed_ms: self.outcome.elapsed.as_millis() as u64,
        }
    }

    /// What the closure costs given what the cache already holds.
    ///
    /// Exact, not approximate: the same authenticated catalog that scheduled the
    /// session measures it, so a frontend can render the number before a byte
    /// moves and be right.
    pub fn estimate(&self) -> zup_acquire::AcquisitionEstimate {
        let session = AcquisitionSession::new(
            self.plan.clone(),
            Arc::clone(&self.cache),
            default_scheduler(),
        );
        session.estimate()
    }

    /// The identity an installation records when this commits.
    pub fn identity(&self) -> zup_core::ReleaseIdentity {
        zup_core::ReleaseIdentity {
            app_id: self.resolved.descriptor.app_id.clone(),
            release: self.resolved.descriptor.release_digest,
            catalog: self.resolved.descriptor.catalog.digest,
            variant: self.resolved.variant.id.clone(),
            manifest: self.resolved.variant.manifest.digest,
            runtime: self.resolved.variant.runtime.map(|runtime| runtime.digest),
            version: self.resolved.descriptor.version.clone(),
            target: self.resolved.variant.target.clone(),
            channel: self.channel.clone(),
            pinned: self.resolved.pinned,
            trust_anchor: self.trust_anchor,
            frontend: self.resolved.variant.frontend.clone(),
            components: self
                .manifest
                .plan
                .installer
                .components
                .iter()
                .map(|component| component.id.to_string())
                .collect(),
        }
    }

    /// Record what the cache retains once the transaction commits.
    ///
    /// Retention marks objects; it never copies them, so a retained object is the
    /// same object the acquisition already published. `temporary` records an
    /// empty set so a later sweep knows there is nothing to keep rather than
    /// assuming there is a closure it cannot find.
    pub fn record_retention(&self, policy: CachePolicy) -> Result<(), std::io::Error> {
        let mut pinned = BTreeSet::new();
        if policy.retains_payload() {
            for entry in &self.manifest.plan.entries {
                pinned.insert(entry.blob);
            }
            for artifact in &self.manifest.plan.prerequisite_artifacts {
                pinned.insert(artifact.blob);
            }
            for artifact in &self.manifest.plan.plugins {
                pinned.insert(artifact.blob);
            }
        }
        let state = RetentionState::record(policy, self.resolved.descriptor.release_digest, pinned);
        write_retention(self.cache.root(), &state)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

/// What a caller wants done with a resolved graph.
///
/// The four operations differ in the lifecycle verb and in which component set is
/// asked for; the closure, the cache, the barrier, and the identity are the same
/// work. Naming the difference is what keeps one code path honest: there is no
/// second downloader to drift.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The lifecycle verb. `None` lets the plan's own scope decide, which is what
    /// a caller with no prior opinion wants.
    pub action: Option<zup_exec::LifecycleAction>,
    /// Component identifiers to turn on.
    pub enable: Vec<String>,
    /// Component identifiers to turn off.
    pub disable: Vec<String>,
    /// A requested install directory, where the plan permits an override.
    pub install_directory: Option<PathBuf>,
    /// The scope, when the caller knows it. A handoff states it; an updater reads
    /// it from the ledger.
    pub scope: Option<zup_core::SelectedScope>,
}

impl Request {
    /// A request for one lifecycle, with nothing narrowed.
    pub fn new(action: zup_exec::LifecycleAction) -> Self {
        Self {
            action: Some(action),
            ..Self::default()
        }
    }

    /// The same request with two component sets applied.
    pub fn components(mut self, enable: &[String], disable: &[String]) -> Self {
        self.enable = enable.to_vec();
        self.disable = disable.to_vec();
        self
    }
}

/// Resolve a release for this machine, compute the closure, and fill the cache.
pub async fn acquire(
    context: TrustContext,
    selection: ComponentSelection<'_>,
    required: Option<&[Sha256Digest]>,
    seeds: &[PathBuf],
    progress: ProgressSink,
) -> Result<Acquired, GraphError> {
    let mut resolver =
        ReleaseResolver::new(context, HostProfile::native(), CachePolicy::Auto, progress)?;
    for (index, seed) in seeds.iter().enumerate() {
        resolver = resolver.with_seed(format!("local source {index}"), seed.clone());
    }
    let resolved = resolver.resolve().await?;
    let manifest = parse_manifest(&resolved)?;
    let plan = match required {
        Some(required) => {
            repair_closure(&resolved, &manifest, &required.iter().copied().collect())?
        }
        None => component_closure(&resolved, &manifest, selection)?,
    };
    barrier(&resolver, resolved, manifest, plan).await
}

/// Compute a closure from a component selection and acquire it.
pub async fn acquire_closure(
    resolver: &ReleaseResolver,
    resolved: ResolvedRelease,
    selection: ComponentSelection<'_>,
) -> Result<Acquired, GraphError> {
    let manifest = parse_manifest(&resolved)?;
    let plan = component_closure(&resolved, &manifest, selection)?;
    barrier(resolver, resolved, manifest, plan).await
}

/// The exact closure a component selection needs.
pub fn component_closure(
    resolved: &ResolvedRelease,
    manifest: &VariantManifest,
    selection: ComponentSelection<'_>,
) -> Result<AcquisitionPlan, GraphError> {
    let entries: Vec<(Sha256Digest, Option<String>)> = manifest
        .plan
        .entries
        .iter()
        .map(|entry| {
            (
                entry.blob,
                entry
                    .component
                    .as_ref()
                    .map(|component| component.to_string()),
            )
        })
        .collect();
    let prerequisites: Vec<(Sha256Digest, String)> = manifest
        .plan
        .prerequisite_artifacts
        .iter()
        .map(|artifact| (artifact.blob, artifact.prerequisite_id.to_string()))
        .collect();
    Ok(resolved.closure(entries, prerequisites, selection)?)
}

/// The exact closure a repair needs: the digests behind drifted resources.
pub fn repair_closure(
    resolved: &ResolvedRelease,
    manifest: &VariantManifest,
    drifted: &BTreeSet<Sha256Digest>,
) -> Result<AcquisitionPlan, GraphError> {
    // Only content the release carries can be reacquired, and only content the
    // plan names is content this machine installed. A digest in neither is a
    // defect in the ledger, not something to fetch - and refusing it is what
    // stops a repair from being talked into installing something it does not own.
    let known = drifted
        .iter()
        .filter(|digest| {
            resolved.catalog.entry(digest).is_some()
                && (manifest
                    .plan
                    .entries
                    .iter()
                    .any(|entry| entry.sha256 == **digest)
                    || manifest
                        .plan
                        .prerequisite_artifacts
                        .iter()
                        .any(|artifact| artifact.sha256 == **digest))
        })
        .copied()
        .collect::<Vec<_>>();
    if known.is_empty() {
        return Err(GraphError::Manifest(
            "no drifted resource names content this release carries".to_owned(),
        ));
    }
    let mut items = Vec::new();
    for digest in known {
        let entry = resolved
            .catalog
            .entry(&digest)
            .expect("filtered to catalogued digests");
        items.push(zup_acquire::AcquisitionItem::new(
            entry.descriptor(ContentKind::Payload),
            ContentReason::Retained,
        ));
    }
    Ok(AcquisitionPlan::build(items)?)
}

/// The variant manifest, checked against the release's own claims.
///
/// TUF already proved the manifest's digest, and the release's fingerprint
/// already described it. This checks that the two documents *agree* - a graph
/// whose manifest names a different target or frontend than the release claims
/// is a defect in the release, and a defect is a refusal, not a re-derivation.
pub fn parse_manifest(resolved: &ResolvedRelease) -> Result<VariantManifest, GraphError> {
    let manifest = VariantManifest::parse(&resolved.manifest)
        .map_err(|error| GraphError::Manifest(error.to_string()))?;
    if manifest.target != resolved.variant.target {
        return Err(GraphError::Manifest(format!(
            "the manifest targets {} but the release claims {}",
            manifest.target, resolved.variant.target
        )));
    }
    if manifest.frontend.as_str() != resolved.variant.frontend {
        return Err(GraphError::Manifest(format!(
            "the manifest presents {} but the release claims {}",
            manifest.frontend, resolved.variant.frontend
        )));
    }
    Ok(manifest)
}

async fn barrier(
    resolver: &ReleaseResolver,
    resolved: ResolvedRelease,
    manifest: VariantManifest,
    plan: AcquisitionPlan,
) -> Result<Acquired, GraphError> {
    let session = AcquisitionSession::new(
        plan.clone(),
        Arc::clone(resolver.cache()),
        default_scheduler(),
    );
    let gate = session
        .run(
            resolver.chain()?,
            std::sync::Arc::new(NeverCancelled),
            resolver.progress(),
        )
        .await?;
    let channel = resolver.context().channel().to_owned();
    let trust_anchor = resolver
        .context()
        .trust_anchor()
        .unwrap_or(Sha256Digest::from_bytes([0; 32]));
    Ok(Acquired {
        resolved,
        manifest,
        plan,
        cache: Arc::clone(resolver.cache()),
        channel,
        trust_anchor,
        outcome: gate.enter(),
    })
}

/// A payload source over the verified cache.
///
/// The plan is the one the graph authenticated, and the digests it names are the
/// keys the cache is addressed by, so a content map is the only translation
/// needed - there is no second index to keep in step.
pub fn payload_source(
    acquired: &Acquired,
) -> Result<zup_bundle::AcquiredPayloadSource, GraphError> {
    let plan = &acquired.manifest.plan;
    let package = zup_bundle::Package::from_bytes(
        zup_bundle::BundleWriter::encode_plan_only_plan(plan, &plan.plugins)
            .map_err(|error| GraphError::Manifest(error.to_string()))?,
    )
    .map_err(|error| GraphError::Manifest(error.to_string()))?;
    Ok(zup_bundle::AcquiredPayloadSource::new(
        zup_acquire::ContentCache::open(
            acquired.cache.root().to_path_buf(),
            acquired.cache.policy(),
        )
        .map_err(|error| GraphError::Acquire(zup_acquire::AcquireError::Cache(error)))?,
        acquired.resolved.catalog.clone(),
        &package,
    ))
}

/// Whether a release is newer than an installed identity.
///
/// Version decides *which* release to prefer; the digest decides whether two
/// claims are the same thing. A rebuild of the same version is not an update,
/// because the lifecycle would refuse it as a same-version upgrade anyway.
pub fn is_newer(identity: &zup_core::ReleaseIdentity, version: &str) -> bool {
    match (
        semver::Version::parse(&identity.version),
        semver::Version::parse(version),
    ) {
        (Ok(installed), Ok(candidate)) => candidate > installed,
        // A version that does not parse is not comparable, so it is not an
        // update. Refusing here is better than a downgrade nobody asked for.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str) -> zup_core::ReleaseIdentity {
        zup_core::ReleaseIdentity {
            app_id: zup_core::AppId::new("com.example.app").expect("valid"),
            release: Sha256Digest::from_bytes([1; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "x64".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: None,
            version: version.to_owned(),
            target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid"),
            channel: "stable".to_owned(),
            pinned: false,
            trust_anchor: Sha256Digest::from_bytes([4; 32]),
            frontend: "gui".to_owned(),
            components: Vec::new(),
        }
    }

    /// What counts as an update. A rebuild of the same version is not one: the bytes
    /// differ, but the lifecycle would refuse a same-version upgrade, so calling it an
    /// update would hand the caller a promise the run cannot keep.
    #[test]
    fn only_a_higher_version_is_an_update() {
        assert!(is_newer(&identity("1.4.0"), "1.5.0"));
        assert!(!is_newer(&identity("1.4.0"), "1.4.0"));
        assert!(!is_newer(&identity("1.4.0"), "1.3.0"), "no downgrades");
        assert!(
            !is_newer(&identity("1.4.0"), "not-a-version"),
            "an unparseable version is not an update"
        );

        let installed = identity("1.4.0");
        let mut rebuild = installed.clone();
        rebuild.release = Sha256Digest::from_bytes([9; 32]);
        assert!(!installed.same_release(&rebuild));
        assert!(!is_newer(&rebuild, "1.4.0"));
    }
}
