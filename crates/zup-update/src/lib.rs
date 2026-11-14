//! TUF-authenticated release resolution and acquisition.
//!
//! Persistent repository metadata records prior observations independently of
//! the content cache. Metadata publication uses the TUF client's datastore.

#[cfg(feature = "bundle")]
pub mod content;

use std::path::PathBuf;
use std::sync::Arc;
use zup_core::ReleasePin;

use sha2::{Digest, Sha256};
use tough::{ExpirationEnforcement, Limits, RepositoryLoader, TargetName};
use url::Url;
use zup_acquire::{
    AcquireError, AcquisitionItem, AcquisitionPlan, ArtifactSource, CachePolicy, ContentCatalog,
    ContentDescriptor, ContentKind, ContentReason, DirectorySource, HostProfile, OnlineTrust,
    ProgressSink, ReleaseDescriptor, ReleaseVariant, SourceChain, select_variant,
};
use zup_acquire_http::{HttpClient, HttpClientConfig, HttpSource, Origin, OriginSet};

pub mod repository_state;

mod pinned;
pub use pinned::{PinnedDownload, fetch_pinned};

/// Bound on any single release-graph document a client will hold in memory.
pub const MAX_DOCUMENT_BYTES: u64 = 256 * 1024 * 1024;

/// Why an authenticated release could not be resolved or satisfied.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid update repository URL: {0}")]
    Url(#[from] url::ParseError),
    /// TUF refused the repository, the metadata, or a signature.
    #[error("TUF repository verification failed: {0}")]
    Tuf(#[source] Box<tough::error::Error>),
    /// The release graph does not name the document this client needs.
    #[error("release target `{0}` is missing")]
    MissingTarget(String),
    /// A document is not the shape the release model requires.
    #[error("invalid release graph: {0}")]
    Graph(String),
    /// The graph authenticated, but it is not this application's.
    #[error("the authenticated release is for {found}, not {expected}")]
    WrongApplication { expected: String, found: String },
    /// The graph authenticated, but it is not the version this client insists on.
    #[error("the authenticated release is {found}, not the pinned {expected}")]
    WrongVersion { expected: String, found: String },
    /// A transport or transfer failure, already reduced to a message.
    #[error("release transport failed: {0}")]
    Transport(String),
    /// The acquisition engine could not satisfy the closure.
    #[error("{0}")]
    Acquire(#[from] AcquireError),
    #[error("release I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("release acquisition was cancelled")]
    Cancelled,
}

impl From<tough::error::Error> for UpdateError {
    fn from(error: tough::error::Error) -> Self {
        Self::Tuf(Box::new(error))
    }
}

/// Which components a closure is for.
///
/// The release names every digest a variant *can* need, the manifest says which
/// component each file belongs to, and this says which of those are wanted. A
/// component with no identifier is required content and is always included,
/// because there is no way for a user to have turned it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSelection<'a> {
    /// Everything the variant can install.
    All,
    /// Exactly these component identifiers.
    Only(&'a [zup_core::ComponentId]),
}

/// Where a client's persistent TUF state lives, and why.
///
/// Rollback protection is the one property TUF cannot provide per request: it is
/// a statement about what this machine has already seen. A datastore in a
/// temporary directory would make every invocation the first invocation, and a
/// publisher could then serve an old `timestamp.json` to any client that had not
/// run lately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustContext {
    /// What the client believes, and where to check it.
    pub trust: OnlineTrust,
    /// The machine's state root. TUF state and the content cache live under it.
    pub state_root: PathBuf,
}

impl TrustContext {
    /// A context from a build's embedded update configuration.
    ///
    /// This is the installed-application path: the trusted root, repository, and
    /// channel come from the build, so an attacker who can write the state root
    /// still cannot redirect the client to a repository of their choosing.
    pub fn from_update_config(
        config: &zup_core::UpdateConfig,
        app_id: zup_core::AppId,
        scope: zup_core::InstallScope,
        state_root: impl Into<PathBuf>,
    ) -> Self {
        let channel = ReleasePin::Channel {
            channel: config.channel.clone(),
        };
        Self {
            trust: OnlineTrust {
                app_id,
                channel: config.channel.clone(),
                repository: config.repository.clone(),
                trusted_root: zup_core::base64_encode(&config.trusted_root),
                pin: channel.clone(),
                mirrors: Vec::new(),
                scope,
            },
            state_root: state_root.into(),
        }
    }

    pub fn from_release(
        config: &zup_core::UpdateConfig,
        identity: &zup_core::ReleaseIdentity,
        scope: zup_core::InstallScope,
        state_root: impl Into<PathBuf>,
    ) -> Result<Self, UpdateError> {
        identity
            .validate()
            .map_err(|error| UpdateError::Graph(error.to_string()))?;
        if zup_core::hash_bytes(&config.trusted_root) != identity.trust_anchor {
            return Err(UpdateError::Graph(
                "the recorded source requires a different update trust anchor".into(),
            ));
        }
        let mut context =
            Self::from_update_config(config, identity.app_id.clone(), scope, state_root);
        context.trust.repository = identity.repository.clone();
        context.trust.channel = identity.channel.clone();
        context.trust.pin = ReleasePin::Release {
            version: identity
                .version
                .parse()
                .map_err(|error| UpdateError::Graph(format!("recorded source version: {error}")))?,
            document: identity.document,
        };
        Ok(context)
    }

    /// Repository trust state is independent of channels and transport locations.
    pub fn datastore_dir(&self) -> Result<PathBuf, UpdateError> {
        let anchor = self
            .trust
            .trust_anchor()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        Ok(self
            .state_root
            .join("repositories")
            .join(self.trust.app_id.as_str())
            .join(anchor.to_hex()))
    }

    /// Where the verified content cache for this context lives.
    ///
    /// One cache per state root, shared by every release, because identity is
    /// cryptographic: a blob a previous version downloaded is the same object
    /// this version needs, and a second copy would cost disk for nothing.
    pub fn cache_dir(&self) -> PathBuf {
        self.state_root.join("content")
    }

    /// The document that answers this context.
    pub fn release_document(&self) -> Result<zup_acquire::RelativeContentPath, UpdateError> {
        self.trust
            .release_document()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))
    }

    /// The digest identifying the publisher's trust anchor.
    pub fn trust_anchor(&self) -> Result<zup_core::Sha256Digest, UpdateError> {
        self.trust
            .trust_anchor()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))
    }
}

use zup_core::Sha256Digest;

/// The TUF loader policy every client uses.
///
/// Stated rather than inherited so a dependency bump cannot silently change what
/// this client will hold in memory.
fn limits() -> Limits {
    Limits {
        max_root_size: 1024 * 1024,
        max_targets_size: 32 * 1024 * 1024,
        max_timestamp_size: 1024 * 1024,
        max_snapshot_size: 4 * 1024 * 1024,
        max_root_updates: 32,
    }
}

/// The authenticated release graph, and the machine's place in it.
///
/// Nothing here is taken on trust from a caller: the release's `release_digest`
/// is recomputed from its own body, the catalog and the variant manifest are the
/// digests that release names, and the runtime is the digest it names *for this
/// host's variant*.
#[derive(Debug, Clone)]
pub struct ResolvedRelease {
    pub descriptor: ReleaseDescriptor,
    pub variant: ReleaseVariant,
    pub catalog: ContentCatalog,
    /// The variant manifest bytes, exactly as the release named them.
    pub manifest: Vec<u8>,
    /// The release document's own bytes, exactly as TUF delivered them.
    ///
    /// Kept because the cache is addressed by the digest of a document's bytes
    /// and a release document names its own fingerprint as a field, so the two
    /// are different numbers. A caller that hands the document to a runtime has
    /// to name it by its bytes, and re-deriving those bytes from a parsed
    /// descriptor would be a second encoding to keep in step with the first.
    pub descriptor_bytes: Vec<u8>,
    /// The digest of `descriptor_bytes`, which is the cache key for the document.
    pub descriptor_digest: zup_core::Sha256Digest,
    /// The document that produced the release, relative to the repository root.
    pub document: String,
    /// Whether this release was addressed by version rather than by channel.
    pub pin: ReleasePin,
    pub repository: String,
    pub trust_anchor: Sha256Digest,
}

impl ResolvedRelease {
    pub fn identity(&self, components: &[zup_core::Component]) -> zup_core::ReleaseIdentity {
        zup_core::ReleaseIdentity {
            app_id: self.descriptor.app_id.clone(),
            release: self.descriptor.release_digest,
            document: self.descriptor_digest,
            catalog: self.descriptor.catalog.digest,
            variant: self.variant.id.clone(),
            manifest: self.variant.manifest.digest,
            runtime: self.variant.runtime.map(|runtime| runtime.digest),
            version: self.descriptor.version.clone(),
            target: self.variant.target,
            channel: self.descriptor.channel.clone(),
            repository: self.repository.clone(),
            pin: self.pin.clone(),
            trust_anchor: self.trust_anchor,
            frontend: self.variant.frontend.clone(),
            components: components
                .iter()
                .map(|component| component.id.to_string())
                .collect(),
        }
    }

    /// The runtime this machine must execute, if the release names one.
    ///
    /// A thin release names one per variant because the machine has to fetch the
    /// right architecture. An offline release carries it inside the installer
    /// and this is `None`, which is how the same closure code serves both.
    pub fn runtime(&self) -> Option<ContentDescriptor> {
        self.descriptor.runtime_descriptor(&self.variant)
    }

    /// Require a runtime, naming what is missing.
    pub fn require_runtime(&self) -> Result<ContentDescriptor, UpdateError> {
        self.runtime().ok_or_else(|| {
            UpdateError::Graph(format!(
                "release {} carries no native runtime for variant `{}`",
                self.descriptor.version, self.variant.id
            ))
        })
    }

    /// The exact closure for a component selection.
    ///
    /// Deduplicated by digest and ordered for the scheduler, so a blob two
    /// components share is one download and a component the user turned off is
    /// zero bytes.
    ///
    /// `declared` is everything the variant needs that is not payload and not a
    /// prerequisite: the images a frontend has to run and the named items its
    /// window expects. The caller says what each one is and this decides nothing
    /// about it, so a new kind of declared content is a new reason rather than a
    /// new argument.
    pub fn closure(
        &self,
        entries: impl IntoIterator<Item = (Sha256Digest, Option<String>)>,
        prerequisites: impl IntoIterator<Item = (Sha256Digest, String)>,
        declared: impl IntoIterator<Item = (Sha256Digest, ContentReason)>,
        selection: ComponentSelection<'_>,
    ) -> Result<AcquisitionPlan, UpdateError> {
        let mut items: Vec<AcquisitionItem> = Vec::new();
        for (digest, component) in entries {
            if !wanted(&component, selection) {
                continue;
            }
            items.push(self.item(digest, reason_for(&component))?);
        }
        for (digest, id) in prerequisites {
            items.push(self.item(digest, ContentReason::Prerequisite { id })?);
        }
        for (digest, reason) in declared {
            items.push(self.item(digest, reason)?);
        }
        Ok(AcquisitionPlan::build(items)?)
    }

    fn item(
        &self,
        digest: Sha256Digest,
        reason: ContentReason,
    ) -> Result<AcquisitionItem, UpdateError> {
        let entry = self.catalog.entry(&digest).ok_or_else(|| {
            UpdateError::Graph(format!(
                "the authenticated catalog does not carry {digest}, which the plan requires"
            ))
        })?;
        Ok(AcquisitionItem::new(
            entry.descriptor(ContentKind::Payload),
            reason,
        ))
    }

    /// Every digest the variant can install, for a caller that needs the
    /// superset rather than a selection - an offline-repair retention set, for
    /// example.
    pub fn full_content(&self) -> Vec<Sha256Digest> {
        self.variant.content.clone()
    }
}

fn wanted(component: &Option<String>, selection: ComponentSelection<'_>) -> bool {
    match selection {
        ComponentSelection::All => true,
        // A file with no component is required content: there is no way for a
        // user to have turned it off, so it is never narrowed away.
        ComponentSelection::Only(_) => match component {
            None => true,
            Some(component) => selection
                .only()
                .is_some_and(|only| only.iter().any(|id| id.as_str() == component)),
        },
    }
}

impl ComponentSelection<'_> {
    fn only(&self) -> Option<&[zup_core::ComponentId]> {
        match self {
            Self::All => None,
            Self::Only(only) => Some(only),
        }
    }
}

fn reason_for(component: &Option<String>) -> ContentReason {
    ContentReason::File {
        component: component.clone(),
    }
}

/// Resolve a release, and be ready to acquire from it.
///
/// One object serves a fresh install, an update, a modify, and a repair. The
/// difference between those four is which component set is asked for and which
/// lifecycle verb is handed on; it is not a different downloader.
pub struct ReleaseResolver {
    context: TrustContext,
    host: HostProfile,
    seeds: Vec<Arc<dyn ArtifactSource>>,
    client: HttpClient,
    cache: Arc<zup_acquire::ContentCache>,
    progress: ProgressSink,
    metadata_store: Option<Arc<dyn tough::MetadataStore>>,
}

impl ReleaseResolver {
    /// A resolver for `context` on `host`, retaining content under `policy`.
    pub fn new(
        context: TrustContext,
        host: HostProfile,
        policy: CachePolicy,
        progress: ProgressSink,
    ) -> Result<Self, UpdateError> {
        let cache = Arc::new(
            zup_acquire::ContentCache::open(context.cache_dir(), policy)
                .map_err(AcquireError::from)?,
        );
        let client = HttpClient::new(&HttpClientConfig::default())
            .map_err(|error| UpdateError::Transport(error.to_string()))?;
        Ok(Self {
            context,
            host,
            seeds: Vec::new(),
            client,
            cache,
            progress,
            metadata_store: None,
        })
    }

    pub fn with_metadata_store(mut self, store: Arc<dyn tough::MetadataStore>) -> Self {
        self.metadata_store = Some(store);
        self
    }

    /// Add a local source ahead of the network.
    ///
    /// This is what makes one artifact work online, from a USB stick, and from an
    /// enterprise share. The seed is untrusted: every descriptor and every blob
    /// still goes through the same digest check, so a mirror can be wrong and can
    /// never be believed.
    pub fn with_seed(mut self, name: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        self.seeds.push(Arc::new(DirectorySource::new(name, root)));
        self
    }

    /// The verified cache this resolver fills.
    pub fn cache(&self) -> &Arc<zup_acquire::ContentCache> {
        &self.cache
    }

    /// The context this resolver authenticates against.
    pub fn context(&self) -> &TrustContext {
        &self.context
    }

    /// The event sink every phase reports into.
    pub fn progress(&self) -> &ProgressSink {
        &self.progress
    }

    /// The ordered source chain: local seeds first, then the origin set.
    ///
    /// Order is a preference. A local share that is complete and healthy costs
    /// zero network bytes; one that is stale or partial costs the same re-fetch
    /// any other stale mirror would, because a digest that is not there is not
    /// there.
    pub fn chain(&self) -> Result<SourceChain, UpdateError> {
        let mut sources: Vec<Arc<dyn ArtifactSource>> = self.seeds.clone();
        let mut remote = Vec::new();
        for (index, address) in std::iter::once(self.context.trust.repository.as_str())
            .chain(self.context.trust.mirrors.iter().map(String::as_str))
            .enumerate()
        {
            let url = repository_base(address)?;
            if url.scheme() == "file" {
                if !remote.is_empty() {
                    sources.push(Arc::new(HttpSource::new(
                        "repository",
                        self.client.clone(),
                        OriginSet::from_origins(std::mem::take(&mut remote)),
                    )));
                }
                let directory = url.to_file_path().map_err(|_| UpdateError::Graph("file repository cannot be represented as a native directory on this platform".into()))?;
                sources.push(Arc::new(DirectorySource::new(
                    format!("repository location {}", index + 1),
                    directory,
                )));
            } else {
                remote.push(Origin::parse(address).map_err(UpdateError::Transport)?);
            }
        }
        if !remote.is_empty() {
            sources.push(Arc::new(HttpSource::new(
                "repository",
                self.client.clone(),
                OriginSet::from_origins(remote),
            )));
        }
        Ok(SourceChain::new(sources))
    }

    /// Authenticate the repository and read the release this context addresses.
    pub async fn resolve(&self) -> Result<ResolvedRelease, UpdateError> {
        self.resolve_with_transport(tough::DefaultTransport::default())
            .await
    }

    pub async fn resolve_with_transport<T: tough::Transport + 'static>(
        &self,
        transport: T,
    ) -> Result<ResolvedRelease, UpdateError> {
        let repository = self.load_repository_with_transport(transport).await?;
        let document = self.context.release_document()?;
        let bytes = read_target(&repository, &document.to_string(), MAX_DOCUMENT_BYTES).await?;
        if let ReleasePin::Release { document, .. } = &self.context.trust.pin {
            check_digest(&bytes, *document, "release document")?;
        }
        let descriptor = ReleaseDescriptor::parse(&bytes)
            .map_err(|error| UpdateError::Graph(error.to_string()))?;
        self.check_belongs(&descriptor)?;

        let catalog_document = zup_acquire::document_path(&descriptor.catalog.digest);
        let catalog_bytes = read_target(
            &repository,
            &catalog_document.to_string(),
            zup_acquire::MAX_CATALOG_BYTES,
        )
        .await?;
        let catalog = ContentCatalog::parse(&catalog_bytes)
            .map_err(|error| UpdateError::Graph(error.to_string()))?;
        check_digest(&catalog_bytes, descriptor.catalog.digest, "content catalog")?;

        let variant = select_variant(&descriptor, &self.host)
            .map_err(|error| UpdateError::Graph(error.to_string()))?
            .clone();
        let manifest_document = zup_acquire::document_path(&variant.manifest.digest);
        let manifest = read_target(
            &repository,
            &manifest_document.to_string(),
            zup_acquire::MAX_METADATA_BYTES,
        )
        .await?;
        check_digest(&manifest, variant.manifest.digest, "variant manifest")?;

        for (kind, digest, document) in [
            (
                ContentKind::Metadata,
                zup_core::hash_bytes(&bytes),
                bytes.as_slice(),
            ),
            (
                ContentKind::Catalog,
                descriptor.catalog.digest,
                catalog_bytes.as_slice(),
            ),
            (
                ContentKind::Metadata,
                variant.manifest.digest,
                manifest.as_slice(),
            ),
        ] {
            let content = ContentDescriptor::stored(kind, digest, document.len() as u64);
            if self
                .cache
                .get(&content, zup_acquire::Verify::Full)
                .map_err(AcquireError::Cache)?
                .is_none()
            {
                let mut output = self.cache.writer(&content).map_err(AcquireError::Cache)?;
                output.write(document).map_err(AcquireError::Cache)?;
                output.commit().map_err(AcquireError::Cache)?;
            }
        }

        self.progress
            .offer(zup_acquire::AcquisitionEvent::ReleaseResolved {
                app_id: descriptor.app_id.to_string(),
                channel: descriptor.channel.clone(),
                version: descriptor.version.clone(),
                release_digest: descriptor.release_digest,
            });
        self.progress
            .offer(zup_acquire::AcquisitionEvent::VariantSelected {
                variant: variant.id.clone(),
                target: variant.target.to_string(),
                compatibility: if self.host.native_execution {
                    "native".to_owned()
                } else {
                    "compatibility".to_owned()
                },
            });

        Ok(ResolvedRelease {
            descriptor,
            variant,
            catalog,
            manifest,
            descriptor_digest: Sha256Digest::from_bytes(Sha256::digest(&bytes).into()),
            descriptor_bytes: bytes,
            document: document.to_string(),
            pin: self.context.trust.pin.clone(),
            repository: self.context.trust.repository.clone(),
            trust_anchor: self.context.trust_anchor()?,
        })
    }

    /// Reject a graph that authenticated but is not this client's.
    fn check_belongs(&self, descriptor: &ReleaseDescriptor) -> Result<(), UpdateError> {
        if descriptor.app_id != self.context.trust.app_id {
            return Err(UpdateError::WrongApplication {
                expected: self.context.trust.app_id.to_string(),
                found: descriptor.app_id.to_string(),
            });
        }
        if descriptor.channel != self.context.trust.channel {
            return Err(UpdateError::Graph(format!(
                "the authenticated release is for channel `{}`, not `{}`",
                descriptor.channel, self.context.trust.channel
            )));
        }
        if let ReleasePin::Release { version, .. } = &self.context.trust.pin
            && descriptor.version != version.to_string()
        {
            return Err(UpdateError::WrongVersion {
                expected: version.to_string(),
                found: descriptor.version.clone(),
            });
        }
        Ok(())
    }

    /// Load the TUF repository against this context's persistent datastore.
    pub async fn load_repository(&self) -> Result<tough::Repository, UpdateError> {
        self.load_repository_with_transport(tough::DefaultTransport::default())
            .await
    }

    async fn load_repository_with_transport<T: tough::Transport + 'static>(
        &self,
        transport: T,
    ) -> Result<tough::Repository, UpdateError> {
        let base = repository_base(&self.context.trust.repository)?;
        let metadata = base.join("metadata/")?;
        let targets = base.join("targets/")?;
        let store = match &self.metadata_store {
            Some(store) => Arc::clone(store),
            None => {
                let context = self.context.clone();
                tokio::task::spawn_blocking(move || repository_state::open_private(&context))
                    .await
                    .map_err(|error| UpdateError::Io(std::io::Error::other(error)))??
            }
        };
        let root = self
            .context
            .trust
            .root_bytes()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        let loader = RepositoryLoader::new(&root, metadata, targets)
            .transport(transport)
            .metadata_store(store)
            .expiration_enforcement(ExpirationEnforcement::Safe)
            .limits(limits());
        Ok(loader.load().await?)
    }
}

/// Where the metadata and targets directories live for a repository URL.
fn repository_base(repository: &str) -> Result<Url, UpdateError> {
    let mut base = Url::parse(repository)?;
    if !matches!(base.scheme(), "https" | "http" | "file")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(UpdateError::Graph(
            "the repository must be an HTTPS, HTTP, or file URL without credentials".into(),
        ));
    }
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    Ok(base)
}

/// Read one TUF target, bounded.
async fn read_target(
    repository: &tough::Repository,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, UpdateError> {
    use futures_util::TryStreamExt;
    let name = name
        .parse::<TargetName>()
        .map_err(|_| UpdateError::MissingTarget(name.to_owned()))?;
    let mut stream = repository
        .read_target(&name)
        .await?
        .ok_or_else(|| UpdateError::MissingTarget(name.raw().to_owned()))?;
    let mut out = Vec::new();
    while let Some(bytes) = stream
        .try_next()
        .await
        .map_err(|error| UpdateError::Transport(error.to_string()))?
    {
        if out.len() as u64 + bytes.len() as u64 > limit {
            return Err(UpdateError::Graph(format!(
                "`{}` exceeds its size limit",
                name.raw()
            )));
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// Check a document against the digest a release authenticated.
///
/// Redundant for a document TUF delivered, because TUF already verified it. Not
/// redundant for one that came from a local seed, and the cost is a hash over a
/// document that is already in memory.
pub fn check_digest(bytes: &[u8], expected: Sha256Digest, what: &str) -> Result<(), UpdateError> {
    let found = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    if found == expected {
        Ok(())
    } else {
        Err(UpdateError::Graph(format!(
            "the {what} hashed to {found}, but the release authenticated {expected}"
        )))
    }
}

/// The engine's scheduler defaults, named once so a caller does not restate a
/// measurement it did not make.
pub const fn default_scheduler() -> zup_acquire::SchedulerConfig {
    zup_acquire::SchedulerConfig {
        total: 12,
        staging: 2,
        progress_interval: std::time::Duration::from_millis(100),
    }
}

/// The scheduler configuration a bootstrapper uses.
///
/// A bootstrapper has one transfer it is blocked on, so there is nothing to
/// overlap. A sequential configuration is the honest one: it cannot waste a
/// thread and it makes the download order the manifest order.
pub const fn bootstrap_scheduler() -> zup_acquire::SchedulerConfig {
    zup_acquire::SchedulerConfig::sequential()
}

/// A re-export for the same reason.
pub type SingleOrigin = Origin;

/// The directory an origin serves one blob under, relative to its base.
pub fn blob_relative_path(digest: &Sha256Digest) -> PathBuf {
    let path = zup_acquire::blob_path(digest).to_string();
    let mut out = PathBuf::new();
    for segment in path.split('/') {
        out.push(segment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_acquire::DocumentRef;

    fn context(repository: &str, channel: &str) -> TrustContext {
        TrustContext {
            trust: OnlineTrust::new(
                zup_core::AppId::new("com.acme.app").expect("valid"),
                channel,
                repository,
                b"root bytes",
                ReleasePin::Channel {
                    channel: channel.to_owned(),
                },
                zup_core::InstallScope::User,
            ),
            state_root: PathBuf::from("/state"),
        }
    }

    fn release() -> ResolvedRelease {
        let mut descriptor = ReleaseDescriptor {
            schema: zup_acquire::RELEASE_SCHEMA,
            app_id: zup_core::AppId::new("com.acme.app").expect("valid"),
            channel: "stable".to_owned(),
            version: "1.4.0".to_owned(),
            release_digest: Sha256Digest::from_bytes([0; 32]),
            catalog: DocumentRef::of(Sha256Digest::from_bytes([1; 32]), 8),
            variants: vec![ReleaseVariant {
                id: "x64".to_owned(),
                target: zup_core::Target::parse("windows-x64").expect("valid triple"),
                platform: "windows".to_owned(),
                frontend: "gui".to_owned(),
                manifest: DocumentRef::of(Sha256Digest::from_bytes([2; 32]), 8),
                runtime: Some(DocumentRef::of(Sha256Digest::from_bytes([3; 32]), 4)),
                preset: Some(DocumentRef::of(Sha256Digest::from_bytes([5; 32]), 6)),
                content: vec![Sha256Digest::from_bytes([4; 32])],
                requirements: Default::default(),
                logical_size: 10,
            }],
            downloads: Vec::new(),
        };
        descriptor.release_digest = descriptor.computed_digest().expect("fingerprints");
        let entries: Vec<zup_acquire::CatalogEntry> = (0u8..6)
            .map(|index| {
                zup_acquire::CatalogEntry::compressed(
                    Sha256Digest::from_bytes([10 + index; 32]),
                    10,
                    100,
                )
            })
            .collect();
        let descriptor_bytes = descriptor.encode().expect("the release encodes");
        let descriptor_digest = Sha256Digest::from_bytes(Sha256::digest(&descriptor_bytes).into());
        ResolvedRelease {
            descriptor,
            descriptor_bytes,
            descriptor_digest,
            variant: ReleaseVariant {
                id: "x64".to_owned(),
                target: zup_core::Target::parse("windows-x64").expect("valid triple"),
                platform: "windows".to_owned(),
                frontend: "gui".to_owned(),
                manifest: DocumentRef::of(Sha256Digest::from_bytes([2; 32]), 8),
                runtime: Some(DocumentRef::of(Sha256Digest::from_bytes([3; 32]), 4)),
                preset: Some(DocumentRef::of(Sha256Digest::from_bytes([5; 32]), 6)),
                content: vec![Sha256Digest::from_bytes([4; 32])],
                requirements: Default::default(),
                logical_size: 10,
            },
            catalog: ContentCatalog::new(entries).expect("catalog"),
            manifest: b"{}".to_vec(),
            document: "releases/stable.json".to_owned(),
            pin: ReleasePin::Channel {
                channel: "stable".into(),
            },
            repository: "https://example.invalid".into(),
            trust_anchor: zup_core::hash_bytes(b"fixture root"),
        }
    }

    #[test]
    fn rollback_history_follows_application_and_root_identity() {
        let one = context("https://a.example.com/acme", "stable");
        let other = context("https://b.example.com/acme", "stable");
        let beta = context("https://a.example.com/acme", "beta");
        let state = |context: &TrustContext| context.datastore_dir().expect("a directory");
        assert_eq!(state(&one), state(&other));
        assert_eq!(state(&one), state(&beta));
        let mut application = one.clone();
        application.trust.app_id = zup_core::AppId::new("com.acme.other").unwrap();
        assert_ne!(state(&one), state(&application));
        let mut root = one.clone();
        root.trust.trusted_root = zup_core::base64_encode(b"different root");
        assert_ne!(state(&one), state(&root));
        assert_eq!(
            one.cache_dir(),
            other.cache_dir(),
            "one cache per state root, shared by every release"
        );
    }
    #[test]
    fn a_repository_url_that_could_leak_a_credential_is_refused() {
        assert!(repository_base("https://user:pw@host/x").is_err());
        assert!(repository_base("ftp://host/x").is_err());
        assert!(repository_base("https://host/x?token=1").is_err());
        assert!(repository_base("https://host/x").is_ok());
    }

    #[test]
    fn a_closure_only_takes_bytes_the_authenticated_catalog_describes() {
        let release = release();
        let error = release
            .closure(
                [(Sha256Digest::from_bytes([0xab; 32]), None)],
                [],
                [],
                ComponentSelection::All,
            )
            .expect_err("the catalog does not carry it");
        assert!(
            error.to_string().contains("authenticated catalog"),
            "{error}"
        );
    }
}
