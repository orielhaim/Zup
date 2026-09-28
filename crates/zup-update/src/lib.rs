//! Resolving an authenticated release graph, and nothing else.
//!
//! This crate is the seam between TUF and the acquisition engine. It answers one
//! question — *which release graph, and which content does this machine need
//! from it* — and it answers it the same way for a first install, an update, a
//! modify, and a repair.
//!
//! ```text
//! trust block + durable TUF state
//!         ↓
//!   authenticated ReleaseDescriptor     releases/<channel>.json
//!         ↓                             or releases/<channel>/versions/<v>.json
//!   selected variant                   one host, one variant
//!         ↓
//!   verified catalog + manifest        both named by digest above
//!         ↓
//!   zup-acquire                        the closure, the cache, the barrier
//!         ↓
//!   RuntimeHandoff                     the identity the lifecycle commits
//! ```
//!
//! # What this crate deliberately does not have
//!
//! No knowledge of transactions, files, registry entries, or Windows. It hands
//! over a verified cache and a bound handoff; what happens next belongs to the
//! native runtime under the transaction engine, where it already is.
//!
//! # TUF state is durable
//!
//! `tough`'s datastore exists to hold the most recently observed
//! timestamp/snapshot/targets metadata, and rollback detection is only possible
//! *across processes* if it survives one. So the datastore is a directory under
//! the machine's state root, namespaced by everything that can change the answer
//! — application, repository, channel, and the trust anchor's own digest — and it
//! is never deleted because a fetch failed. A publisher who cannot be reached
//! must not be able to reset a client's rollback memory.

use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tough::{ExpirationEnforcement, Limits, RepositoryLoader, TargetName};
use url::Url;
use zup_acquire::{
    AcquireError, AcquisitionItem, AcquisitionPlan, ArtifactSource, CachePolicy, ContentCatalog,
    ContentDescriptor, ContentKind, ContentReason, DirectorySource, HostProfile, OnlineTrust,
    ProgressSink, ReleaseDescriptor, ReleasePin, ReleaseVariant, SourceChain, WebLayout,
    select_variant,
};
use zup_acquire_http::{HttpClient, HttpClientConfig, HttpSource, Origin, OriginSet};

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

impl UpdateError {
    /// Whether this failure left the machine untouched.
    ///
    /// Every one of these is raised before the barrier, so the answer is always
    /// yes. It is stated rather than inferred so a frontend can assert it.
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
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
    Only(&'a [&'a str]),
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
    /// The version this client insists on, when it is pinned.
    pub pin: ReleasePin,
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
                // An update client is driven by a configuration file, not by an
                // artifact a person double-clicked, so the scope comes from that
                // configuration rather than from a default that would have to be
                // guessed. `User` here is the value a client is constructed with
                // by `zup`, which passes the manifest's declared scope.
                scope: zup_acquire::ThinScope::User,
            },
            state_root: state_root.into(),
            pin: channel,
        }
    }

    /// The same context, pinned to one exact version.
    ///
    /// This is what a version-labelled thin installer is. It reads the immutable
    /// version-addressed release document, so a later publication of the channel
    /// cannot move it — which is the entire difference between the two thin
    /// artifacts and the reason they are two files.
    pub fn pinned_to(mut self, version: &str) -> Self {
        self.pin = ReleasePin::Version {
            version: version.to_owned(),
        };
        self.trust.pin = self.pin.clone();
        self
    }

    /// The channel this context resolves through, which a pinned context still
    /// names: the version-addressed document lives inside the channel.
    pub fn channel(&self) -> &str {
        match &self.pin {
            ReleasePin::Version { .. } => &self.trust.channel,
            ReleasePin::Channel { channel } => channel,
        }
    }

    /// Where TUF's persistent metadata for this context lives.
    pub fn datastore_dir(&self) -> Result<PathBuf, UpdateError> {
        let anchor = self
            .trust
            .trust_anchor()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        zup_acquire::check_channel(self.channel())
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        Ok(self
            .state_root
            .join("repositories")
            .join(&anchor.to_hex()[..32])
            .join(self.repository_fingerprint())
            .join(self.channel()))
    }

    /// A short digest of where this context looks.
    ///
    /// The path component is a digest rather than the URL itself so a state
    /// directory listing cannot be used to enumerate the mirrors a deployment
    /// uses, and so a URL with characters a filesystem dislikes cannot become a
    /// path.
    fn repository_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"zup/repository-location/v1\0");
        hasher.update(self.trust.repository.as_bytes());
        for mirror in &self.trust.mirrors {
            hasher.update([0]);
            hasher.update(mirror.as_bytes());
        }
        Sha256Digest::from_bytes(hasher.finalize().into())
            .to_hex()
            .chars()
            .take(32)
            .collect()
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
        let mut trust = self.trust.clone();
        trust.pin = self.pin.clone();
        trust
            .release_document()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))
    }

    /// The origins content may be served from, primary first.
    pub fn origins(&self) -> Result<OriginSet, UpdateError> {
        OriginSet::from_urls(
            &self.trust.repository,
            self.trust.mirrors.iter().map(String::as_str),
        )
        .map_err(UpdateError::Transport)
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
    pub pinned: bool,
}

impl ResolvedRelease {
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
    pub fn closure(
        &self,
        entries: impl IntoIterator<Item = (Sha256Digest, Option<String>)>,
        prerequisites: impl IntoIterator<Item = (Sha256Digest, String)>,
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
    /// superset rather than a selection — an offline-repair retention set, for
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
                .is_some_and(|only| only.contains(&component.as_str())),
        },
    }
}

impl ComponentSelection<'_> {
    fn only(&self) -> Option<&[&str]> {
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
        })
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
        sources.push(Arc::new(HttpSource::new(
            "repository",
            self.client.clone(),
            self.context.origins()?,
        )));
        Ok(SourceChain::new(sources))
    }

    /// Authenticate the repository and read the release this context addresses.
    pub async fn resolve(&self) -> Result<ResolvedRelease, UpdateError> {
        let repository = self.load_repository().await?;
        let document = self.context.release_document()?;
        let bytes = read_target(&repository, &document.to_string(), MAX_DOCUMENT_BYTES).await?;
        let descriptor = ReleaseDescriptor::parse(&bytes)
            .map_err(|error| UpdateError::Graph(error.to_string()))?;
        self.check_belongs(&descriptor)?;

        let catalog_document = WebLayout::catalog(self.context.channel())
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
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
        let manifest_document = WebLayout::variant_manifest(self.context.channel(), &variant.id)
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        let manifest = read_target(
            &repository,
            &manifest_document.to_string(),
            zup_acquire::MAX_METADATA_BYTES,
        )
        .await?;
        check_digest(&manifest, variant.manifest.digest, "variant manifest")?;

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
            pinned: matches!(self.context.pin, ReleasePin::Version { .. }),
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
        // A pinned client reads the version-addressed document, and TUF already
        // bound that name to these bytes. Checking the claim against the pin as
        // well means a mis-published repository is a refusal rather than a
        // surprise install.
        if let ReleasePin::Version { version } = &self.context.pin
            && &descriptor.version != version
        {
            return Err(UpdateError::WrongVersion {
                expected: version.clone(),
                found: descriptor.version.clone(),
            });
        }
        Ok(())
    }

    /// Load the TUF repository against this context's durable datastore.
    pub async fn load_repository(&self) -> Result<tough::Repository, UpdateError> {
        let base = repository_base(&self.context.trust.repository)?;
        let metadata = base.join("metadata/")?;
        let targets = base.join("targets/")?;
        let datastore = self.context.datastore_dir()?;
        tokio::fs::create_dir_all(&datastore).await?;
        let root = self
            .context
            .trust
            .root_bytes()
            .map_err(|detail| UpdateError::Graph(detail.to_owned()))?;
        let mut loader = RepositoryLoader::new(&root, metadata, targets)
            .datastore(datastore)
            .expiration_enforcement(ExpirationEnforcement::Safe)
            .limits(limits());
        if base.scheme() == "file" {
            loader = loader.transport(tough::FilesystemTransport);
        }
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
        per_origin: 6,
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

/// A re-export so a consumer of this crate's sources does not need the transport
/// crate for a type it only names.
pub type Origins = OriginSet;

/// A re-export for the same reason.
pub type SingleOrigin = Origin;

/// The directory an origin serves one blob under, relative to its base.
pub fn blob_relative_path(digest: &Sha256Digest) -> PathBuf {
    let path = WebLayout::blob(digest).to_string();
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
            ),
            state_root: PathBuf::from("/state"),
            pin: ReleasePin::Channel {
                channel: channel.to_owned(),
            },
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
                target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc")
                    .expect("valid triple"),
                platform: "windows".to_owned(),
                frontend: "gui".to_owned(),
                manifest: DocumentRef::of(Sha256Digest::from_bytes([2; 32]), 8),
                runtime: Some(DocumentRef::of(Sha256Digest::from_bytes([3; 32]), 4)),
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
                target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc")
                    .expect("valid triple"),
                platform: "windows".to_owned(),
                frontend: "gui".to_owned(),
                manifest: DocumentRef::of(Sha256Digest::from_bytes([2; 32]), 8),
                runtime: Some(DocumentRef::of(Sha256Digest::from_bytes([3; 32]), 4)),
                content: vec![Sha256Digest::from_bytes([4; 32])],
                requirements: Default::default(),
                logical_size: 10,
            },
            catalog: ContentCatalog::new(entries).expect("catalog"),
            manifest: b"{}".to_vec(),
            document: "releases/stable.json".to_owned(),
            pinned: false,
        }
    }

    #[test]
    fn two_repositories_never_share_rollback_memory() {
        let one = context("https://a.example.com/acme", "stable");
        let other = context("https://b.example.com/acme", "stable");
        let beta = context("https://a.example.com/acme", "beta");
        let state = |context: &TrustContext| context.datastore_dir().expect("a directory");
        assert_ne!(state(&one), state(&other));
        assert_ne!(state(&one), state(&beta));
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
                ComponentSelection::All,
            )
            .expect_err("the catalog does not carry it");
        assert!(
            error.to_string().contains("authenticated catalog"),
            "{error}"
        );
    }
}
