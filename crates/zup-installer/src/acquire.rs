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
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}

pub struct Acquired {
    pub resolved: ResolvedRelease,
    pub manifest: VariantManifest,
    pub plan: AcquisitionPlan,
    pub cache: Arc<zup_acquire::ContentCache>,
    pub channel: String,
    pub trust_anchor: Sha256Digest,
    pub outcome: AcquisitionOutcome,
}

impl Acquired {
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

    pub fn estimate(&self) -> zup_acquire::AcquisitionEstimate {
        let session = AcquisitionSession::new(
            self.plan.clone(),
            Arc::clone(&self.cache),
            default_scheduler(),
        );
        session.estimate()
    }

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

    /// Retention marks objects; it never copies them, so a retained object is the
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
            if let Some(preset) = self.manifest.preset.as_ref() {
                pinned.insert(preset.digest);
            }
            for asset in &self.manifest.plan.ui_assets {
                pinned.insert(asset.sha256);
            }
        }
        let state = RetentionState::record(policy, self.resolved.descriptor.release_digest, pinned);
        write_retention(self.cache.root(), &state)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub action: Option<zup_exec::LifecycleAction>,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
    pub scope: Option<zup_core::SelectedScope>,
}

impl Request {
    pub fn new(action: zup_exec::LifecycleAction) -> Self {
        Self {
            action: Some(action),
            ..Self::default()
        }
    }

    pub fn components(mut self, enable: &[String], disable: &[String]) -> Self {
        self.enable = enable.to_vec();
        self.disable = disable.to_vec();
        self
    }
}

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

pub async fn acquire_closure(
    resolver: &ReleaseResolver,
    resolved: ResolvedRelease,
    selection: ComponentSelection<'_>,
) -> Result<Acquired, GraphError> {
    let manifest = parse_manifest(&resolved)?;
    let plan = component_closure(&resolved, &manifest, selection)?;
    barrier(resolver, resolved, manifest, plan).await
}

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
    Ok(resolved.closure(
        entries,
        prerequisites,
        declared_content(manifest)?,
        selection,
    )?)
}

pub fn declared_content(
    manifest: &VariantManifest,
) -> Result<Vec<(Sha256Digest, ContentReason)>, GraphError> {
    let mut declared: Vec<(Sha256Digest, ContentReason)> = manifest
        .plan
        .ui_assets
        .iter()
        .map(|asset| {
            (
                asset.sha256,
                ContentReason::PresetAsset {
                    name: asset.name.to_string(),
                },
            )
        })
        .collect();
    match (
        manifest.plan.installer.preset.as_ref(),
        manifest.preset.as_ref(),
    ) {
        (Some(_), Some(preset)) => {
            declared.push((preset.digest, ContentReason::Preset));
            Ok(declared)
        }
        (Some(_), None) => Err(GraphError::Manifest(format!(
            "variant `{}` presents a window but the release carries no native image for it",
            manifest.target
        ))),
        (None, Some(_)) => Err(GraphError::Manifest(format!(
            "variant `{}` carries a window image for a plan that presents none",
            manifest.target
        ))),
        (None, None) => Ok(declared),
    }
}

pub fn repair_closure(
    resolved: &ResolvedRelease,
    manifest: &VariantManifest,
    drifted: &BTreeSet<Sha256Digest>,
) -> Result<AcquisitionPlan, GraphError> {
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
                        .any(|artifact| artifact.sha256 == **digest)
                    || manifest
                        .plan
                        .ui_assets
                        .iter()
                        .any(|asset| asset.sha256 == **digest)
                    || manifest
                        .preset
                        .as_ref()
                        .is_some_and(|preset| preset.digest == **digest))
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

pub fn is_newer(identity: &zup_core::ReleaseIdentity, version: &str) -> bool {
    match (
        semver::Version::parse(&identity.version),
        semver::Version::parse(version),
    ) {
        (Ok(installed), Ok(candidate)) => candidate > installed,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proved here rather than from a test target because the invariant spans the

    const PRESET: &[u8] = b"the preset executable";
    const LOGO: &[u8] = b"<svg/>";
    const HERO: &[u8] = b"\x89PNG\r\n";

    fn target_triple() -> zup_core::TargetTriple {
        zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target")
    }

    fn asset(name: &str, content: &[u8]) -> zup_core::PresetAsset {
        zup_core::PresetAsset {
            name: zup_core::NonEmptyString::new(name).expect("a name"),
            size: content.len() as u64,
            sha256: zup_core::hash_bytes(content),
        }
    }

    struct Release {
        manifest: VariantManifest,
        catalog: zup_acquire::ContentCatalog,
    }

    impl Release {
        fn closure(&self) -> Result<zup_acquire::AcquisitionPlan, String> {
            let declared = declared_content(&self.manifest).map_err(|error| error.to_string())?;
            let mut items = Vec::new();
            for (digest, reason) in declared {
                let entry = self
                    .catalog
                    .entry(&digest)
                    .ok_or_else(|| format!("the authenticated catalog does not carry {digest}"))?;
                items.push(zup_acquire::AcquisitionItem::new(
                    entry.descriptor(zup_acquire::ContentKind::Payload),
                    reason,
                ));
            }
            zup_acquire::AcquisitionPlan::build(items).map_err(|error| error.to_string())
        }
    }

    fn release(
        window: bool,
        preset_bytes: &[u8],
        assets: &[(&str, &[u8])],
        carry: &[&[u8]],
    ) -> Release {
        let mut settings = serde_json::Map::new();
        let mut declared = Vec::new();
        for (name, content) in assets {
            settings.insert((*name).to_owned(), serde_json::Value::from(*name));
            declared.push(asset(name, content));
        }
        let target = target_triple();
        let manifest = zup_artifact::VariantManifest {
            schema: zup_artifact::VARIANT_MANIFEST_SCHEMA,
            required_features: zup_artifact::FEATURE_VARIANT_MANIFESTS,
            target: target.clone(),
            platform: zup_artifact::Platform::from_triple(&target),
            frontend: zup_core::Frontend::Gui,
            runtime: Some(zup_artifact::Descriptor::of(
                zup_artifact::MediaType::RUNTIME,
                b"a runtime image",
            )),
            preset: window.then(|| {
                zup_artifact::Descriptor::of(zup_artifact::MediaType::PRESET, preset_bytes)
            }),
            requirements: zup_artifact::VariantRequirements::default(),
            plan: zup_bundle::PortableBuildPlan {
                installer: zup_core::Installer {
                    app: zup_core::App {
                        id: zup_core::AppId::new("com.acme.preset").expect("a valid id"),
                        name: zup_core::NonEmptyString::new("Acme").expect("a name"),
                        version: semver::Version::parse("1.0.0").expect("a version"),
                        publisher: None,
                        main: None,
                        description: None,
                    },
                    target,
                    frontend: zup_core::Frontend::Gui,
                    preset: window.then(|| zup_core::PresetRuntime {
                        name: zup_core::NonEmptyString::new("aurora").expect("a name"),
                        version: semver::Version::parse("1.4.2").expect("a version"),
                        protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
                        required_capabilities: vec!["components".to_owned()],
                        settings: serde_json::Value::Object(settings),
                        assets: declared.clone(),
                    }),
                    updates: None,
                    install: zup_core::Install {
                        scope: zup_core::InstallScope::User,
                        directory: zup_core::InstallDirectory {
                            user: Some(zup_core::Template::parse("${install}").expect("a dir")),
                            machine: None,
                        },
                        allow_directory_override: false,
                    },
                    prerequisites: Vec::new(),
                    components: Vec::new(),
                    component_groups: Vec::new(),
                    plugins: Vec::new(),
                    files: Vec::new(),
                    launchers: Vec::new(),
                    path: Vec::new(),
                    services: Vec::new(),
                    protocols: Vec::new(),
                    file_associations: Vec::new(),
                },
                entries: Vec::new(),
                prerequisite_artifacts: Vec::new(),
                ui_assets: declared,
                plugins: Vec::new(),
                total_size: 0,
            },
            logical_size: 0,
        };
        let mut manifest = manifest;
        manifest.logical_size = manifest
            .runtime
            .iter()
            .chain(manifest.preset.as_ref())
            .map(|image| image.size)
            .sum();
        let manifest = VariantManifest::parse(&serde_json::to_vec(&manifest).expect("serializes"))
            .expect("a manifest this crate wrote is one it reads");

        let mut entries: Vec<zup_acquire::CatalogEntry> = carry
            .iter()
            .copied()
            .chain(window.then_some(preset_bytes))
            .chain(assets.iter().map(|(_, content)| *content))
            .map(|content| {
                zup_acquire::CatalogEntry::stored(
                    zup_core::hash_bytes(content),
                    content.len() as u64,
                )
            })
            .collect();
        entries.sort_by_key(|entry| entry.digest);
        entries.dedup_by_key(|entry| entry.digest);
        Release {
            manifest,
            catalog: zup_acquire::ContentCatalog::new(entries).expect("a well formed catalog"),
        }
    }

    #[test]
    fn a_window_with_no_assets_still_fetches_its_executable() {
        let release = release(true, PRESET, &[], &[]);
        let plan = release.closure().expect("a closure");
        assert_eq!(plan.len(), 1, "the executable and nothing else");
        assert_eq!(plan.items()[0].reason, ContentReason::Preset);
    }

    #[test]
    fn every_named_asset_is_in_the_closure() {
        let release = release(
            true,
            PRESET,
            &[("branding/logo.svg", LOGO), ("branding/hero.png", HERO)],
            &[],
        );
        let plan = release.closure().expect("a closure");
        assert_eq!(plan.len(), 3, "the executable and both assets");
        let mut named: Vec<&str> = plan
            .items()
            .iter()
            .filter_map(|item| match &item.reason {
                ContentReason::PresetAsset { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        named.sort_unstable();
        assert_eq!(
            named,
            ["branding/hero.png", "branding/logo.svg"],
            "each asset keeps the name its settings used, whichever order the scheduler chose"
        );
        assert!(
            plan.items()
                .iter()
                .any(|item| item.reason == ContentReason::Preset),
            "and the executable is fetched beside them"
        );
        assert!(
            plan.wire_size_by_group().contains_key("preset"),
            "and a progress report can account for the whole preset at once"
        );
    }

    #[test]
    fn one_content_under_two_names_is_fetched_once() {
        let release = release(
            true,
            PRESET,
            &[
                ("branding/logo.svg", LOGO),
                ("branding/logo-mask.svg", LOGO),
            ],
            &[],
        );
        let plan = release.closure().expect("a closure");
        assert_eq!(
            plan.len(),
            2,
            "the executable and the one piece of content the two names share"
        );
    }

    /// The invariant is that the declared set is not a function of the selection
    #[test]
    fn a_window_is_never_narrowed_away() {
        let release = release(true, PRESET, &[("branding/logo.svg", LOGO)], &[]);
        let mut reasons = release
            .closure()
            .expect("a closure")
            .items()
            .iter()
            .map(|item| item.reason.clone())
            .collect::<Vec<_>>();
        reasons.sort();
        assert_eq!(
            reasons,
            [
                ContentReason::Preset,
                ContentReason::PresetAsset {
                    name: "branding/logo.svg".to_owned()
                }
            ],
            "the window and its asset are required content, and nothing narrows them"
        );
        assert!(
            reasons
                .iter()
                .all(|reason| !matches!(reason, ContentReason::File { .. })),
            "and none of them is payload, so there is no component to select"
        );
    }

    #[rstest::rstest]
    #[case::an_asset_the_catalog_does_not_carry(
        Some("branding/absent.svg"),
        "the authenticated catalog does not carry"
    )]
    #[case::an_executable_the_release_does_not_carry(None, "carries no native image for it")]
    fn content_a_release_does_not_carry_is_refused(
        #[case] absent: Option<&str>,
        #[case] expected: &str,
    ) {
        let mut release = release(true, PRESET, &[("branding/logo.svg", LOGO)], &[]);
        match absent {
            Some(name) => release
                .manifest
                .plan
                .ui_assets
                .push(asset(name, b"never shipped")),
            None => release.manifest.preset = None,
        }
        let error = release
            .closure()
            .expect_err("a release that names content it does not carry");
        assert!(
            error.contains(expected),
            "and the refusal says what is missing: {error}"
        );
    }

    #[test]
    fn a_variant_with_no_window_declares_no_content() {
        let release = release(false, PRESET, &[], &[b"an application payload"]);
        assert!(
            declared_content(&release.manifest)
                .expect("valid")
                .is_empty(),
            "so there is nothing a machine would fetch for a window that is not there"
        );
    }

    #[test]
    fn a_window_that_did_not_change_is_the_same_content() {
        let digests = |preset_bytes: &[u8], asset: &[u8]| {
            release(true, preset_bytes, &[("branding/logo.svg", asset)], &[])
                .closure()
                .expect("a closure")
                .digests()
        };
        let first = digests(PRESET, LOGO);
        let again = digests(PRESET, LOGO);
        assert_eq!(
            first, again,
            "an unchanged window fetches the same bytes, so it is one download"
        );
        assert_ne!(
            first,
            digests(PRESET, HERO),
            "a changed asset is different content"
        );
        assert_ne!(
            first,
            digests(b"a new preset", LOGO),
            "and so is a new executable"
        );
    }

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
