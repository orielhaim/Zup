//! The authenticated release graph, described without naming a transport.
//!
//! TUF authenticates this document in the online case and a platform artifact
//! signature authenticates the equivalent bytes in the offline case. Neither
//! choice leaks in here: a [`ReleaseDescriptor`] is a set of claims, and the
//! thing that vouches for them is the caller's problem.
//!
//! ```text
//! ReleaseDescriptor
//!   ├── release_digest        identity of this exact graph
//!   ├── catalog               digest + length of the content catalog
//!   └── variants[]
//!         ├── manifest        digest + length of the variant manifest
//!         ├── runtime         digest + length of the native runtime, when there is one
//!         └── content         digests this variant needs
//! ```
//!
//! A version number is never identity. Two builds can carry the same
//! `1.4.0` and install different bytes on the same machine, so every claim
//! that survives an upgrade is a digest.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::{AppId, Sha256Digest, TargetTriple};

use crate::descriptor::{ContentDescriptor, ContentKind, RelativeContentPath};
use crate::layout::{check_channel, check_segment};

/// Current release descriptor schema.
pub const RELEASE_SCHEMA: u32 = 1;

/// How a release names itself, which decides whether a bootstrapper resolves
/// it now or was built for one exact graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pin", rename_all = "snake_case")]
pub enum ReleasePin {
    /// Exactly this release. A thin installer built this way always installs
    /// this graph and never silently becomes a later one.
    Version { version: String },
    /// Whatever the channel currently resolves to, authenticated at runtime.
    Channel { channel: String },
}

impl ReleasePin {
    /// A short label for a build output and a window title.
    pub fn label(&self) -> String {
        match self {
            Self::Version { version } => version.clone(),
            Self::Channel { channel } => channel.clone(),
        }
    }

    /// Whether this pin names one graph forever.
    pub const fn is_pinned(&self) -> bool {
        matches!(self, Self::Version { .. })
    }

    /// The channel this pin resolves through.
    pub fn channel(&self) -> &str {
        match self {
            Self::Version { .. } => "",
            Self::Channel { channel } => channel,
        }
    }
}

/// One claim inside a release descriptor: a document identified by its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRef {
    pub digest: Sha256Digest,
    pub size: u64,
}

impl DocumentRef {
    /// Name a document by its bytes.
    pub fn of(digest: Sha256Digest, size: u64) -> Self {
        Self { digest, size }
    }

    /// The acquisition descriptor for this document.
    pub fn descriptor(&self, kind: ContentKind) -> ContentDescriptor {
        ContentDescriptor::of_document_kind(kind, self.digest, self.size)
    }

    /// Check the document against the bound its kind declares.
    pub fn validate(&self, kind: ContentKind) -> Result<(), &'static str> {
        if self.size == 0 {
            return Err("a release document cannot be zero bytes");
        }
        if self.size > kind.size_limit() {
            return Err("a release document exceeds the size limit for its kind");
        }
        Ok(())
    }
}

/// What a machine needs to know about itself to run one variant.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequirements {
    /// The variant cannot run under emulation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub native_execution: bool,
    /// The variant needs a capability the host may lack. The names are
    /// opaque here on purpose: this crate does not decide what a machine can
    /// do, it only carries the claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One machine's worth of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseVariant {
    /// Stable identifier within the release.
    pub id: String,
    pub target: TargetTriple,
    /// The platform the variant runs on, in the artifact graph's vocabulary.
    pub platform: String,
    /// `gui` or `console`, which decides what the user sees.
    pub frontend: String,
    /// The variant manifest: the installer's content plan, digest-addressed.
    pub manifest: DocumentRef,
    /// The native runtime to hand control to, digest-addressed.
    ///
    /// A thin release has one per variant because the machine has to fetch the
    /// right one; an offline release carries it inside the artifact instead and
    /// this stays `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<DocumentRef>,
    /// Every digest the variant can need, in ascending order.
    ///
    /// This is the superset. The exact closure a transaction wants is this set
    /// narrowed by the components the user selected, and narrowing needs the
    /// manifest, which the release names.
    pub content: Vec<Sha256Digest>,
    #[serde(default, skip_serializing_if = "ReleaseRequirements::is_empty")]
    pub requirements: ReleaseRequirements,
    /// Uncompressed bytes of the whole variant, for an estimate before
    /// anything is selected.
    #[serde(default)]
    pub logical_size: u64,
}

impl ReleaseRequirements {
    fn is_empty(&self) -> bool {
        !self.native_execution && self.capabilities.is_empty()
    }
}

/// The files a human can download, kept in the graph rather than beside it.
///
/// The offline installer stays a first-class output because enterprise and
/// disconnected installs still want one file. It is a *claim* about a file,
/// not a separate package representation, and an updater never needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDownload {
    /// What kind of file this is.
    pub kind: ReleaseDownloadKind,
    /// Path relative to the release root.
    pub path: String,
    pub descriptor: DocumentRef,
    /// The target it serves, when it serves one machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseDownloadKind {
    /// A complete installer carrying every byte the machine needs.
    OfflineInstaller,
    /// A thin installer that resolves a release and fetches the rest.
    ThinInstaller,
}

/// The authenticated root of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDescriptor {
    pub schema: u32,
    pub app_id: AppId,
    pub channel: String,
    pub version: String,
    /// Identity of this exact graph.
    ///
    /// Computed over the canonical body of every other field, so two releases
    /// that differ in one byte have different fingerprints and an installation
    /// can name the graph that produced it.
    pub release_digest: Sha256Digest,
    /// The digest-to-size catalog for this release's content.
    pub catalog: DocumentRef,
    /// Variants, sorted by id.
    pub variants: Vec<ReleaseVariant>,
    /// Files a human can download.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downloads: Vec<ReleaseDownload>,
}

impl ReleaseDescriptor {
    /// Bound on the number of variants one release may name.
    pub const MAX_VARIANTS: usize = 64;

    /// Bound on the number of digests one variant may name.
    pub const MAX_VARIANT_CONTENT: usize = 4_000_000;

    /// Encode a release as canonical JSON.
    pub fn encode(&self) -> Result<Vec<u8>, crate::AcquireError> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > crate::MAX_METADATA_BYTES {
            return Err(crate::AcquireError::TooLarge {
                kind: crate::ContentKind::Metadata.as_str(),
                size: bytes.len() as u64,
                limit: crate::MAX_METADATA_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Parse a release, enforcing its bound before deserializing.
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::AcquireError> {
        if bytes.len() as u64 > crate::MAX_METADATA_BYTES {
            return Err(crate::AcquireError::TooLarge {
                kind: crate::ContentKind::Metadata.as_str(),
                size: bytes.len() as u64,
                limit: crate::MAX_METADATA_BYTES,
            });
        }
        let release: Self = serde_json::from_slice(bytes)?;
        release.validate()?;
        Ok(release)
    }

    /// Reject a release that is inconsistent with its own claims.
    ///
    /// The `release_digest` is recomputed rather than compared field by field,
    /// so a release cannot assert a fingerprint that does not describe it.
    pub fn validate(&self) -> Result<(), crate::AcquireError> {
        if self.schema != RELEASE_SCHEMA {
            return Err(crate::AcquireError::Descriptor(
                "unsupported release descriptor schema",
            ));
        }
        check_channel(&self.channel).map_err(crate::AcquireError::Descriptor)?;
        if self.version.is_empty() {
            return Err(crate::AcquireError::Descriptor(
                "a release names no version",
            ));
        }
        self.catalog
            .validate(ContentKind::Catalog)
            .map_err(crate::AcquireError::Descriptor)?;
        if self.variants.is_empty() {
            return Err(crate::AcquireError::Descriptor(
                "a release names no variants",
            ));
        }
        if self.variants.len() > Self::MAX_VARIANTS {
            return Err(crate::AcquireError::TooManyItems {
                count: self.variants.len(),
                limit: Self::MAX_VARIANTS,
            });
        }
        let mut previous: Option<&str> = None;
        let mut targets = BTreeSet::new();
        for variant in &self.variants {
            check_segment(&variant.id).map_err(crate::AcquireError::Descriptor)?;
            if let Some(previous) = previous
                && variant.id.as_str() <= previous
            {
                return Err(crate::AcquireError::Descriptor(
                    "release variants are not sorted by strictly ascending id",
                ));
            }
            previous = Some(&variant.id);
            if variant.target.as_str().is_empty() {
                return Err(crate::AcquireError::Descriptor(
                    "a release variant names no target",
                ));
            }
            if variant.platform.is_empty() {
                return Err(crate::AcquireError::Descriptor(
                    "a release variant names no platform",
                ));
            }
            if variant.frontend.is_empty() {
                return Err(crate::AcquireError::Descriptor(
                    "a release variant names no frontend",
                ));
            }
            if !targets.insert(variant.target.clone()) {
                return Err(crate::AcquireError::Descriptor(
                    "two release variants name the same target",
                ));
            }
            variant
                .manifest
                .validate(ContentKind::Metadata)
                .map_err(crate::AcquireError::Descriptor)?;
            if let Some(runtime) = &variant.runtime {
                runtime
                    .validate(ContentKind::Runtime)
                    .map_err(crate::AcquireError::Descriptor)?;
            }
            if variant.content.is_empty() {
                return Err(crate::AcquireError::Descriptor(
                    "a release variant names no content",
                ));
            }
            if variant.content.len() > Self::MAX_VARIANT_CONTENT {
                return Err(crate::AcquireError::TooManyItems {
                    count: variant.content.len(),
                    limit: Self::MAX_VARIANT_CONTENT,
                });
            }
            let mut previous = None;
            for digest in &variant.content {
                if let Some(previous) = previous
                    && digest <= previous
                {
                    return Err(crate::AcquireError::Descriptor(
                        "variant content is not sorted by strictly ascending digest",
                    ));
                }
                previous = Some(digest);
            }
        }
        for download in &self.downloads {
            if let Some(variant) = &download.variant
                && self.variant(variant).is_none()
            {
                return Err(crate::AcquireError::Descriptor(
                    "a release download names a variant the release does not carry",
                ));
            }
        }
        if self.computed_digest()? != self.release_digest {
            return Err(crate::AcquireError::Descriptor(
                "release fingerprint does not describe this release",
            ));
        }
        Ok(())
    }

    /// The fingerprint of this release's content, independent of the field
    /// that claims it.
    pub fn computed_digest(&self) -> Result<Sha256Digest, crate::AcquireError> {
        let body = ReleaseBody {
            app_id: &self.app_id,
            channel: &self.channel,
            version: &self.version,
            catalog: &self.catalog,
            variants: &self.variants,
            downloads: &self.downloads,
        };
        let bytes = serde_json::to_vec(&body)?;
        Ok(Sha256Digest::from_bytes(Sha256::digest(&bytes).into()))
    }

    /// One variant by id.
    pub fn variant(&self, id: &str) -> Option<&ReleaseVariant> {
        self.variants
            .binary_search_by(|variant| variant.id.as_str().cmp(id))
            .ok()
            .map(|index| &self.variants[index])
    }

    /// One variant by target triple.
    pub fn variant_for_target(&self, target: &TargetTriple) -> Option<&ReleaseVariant> {
        self.variants
            .iter()
            .find(|variant| &variant.target == target)
    }

    /// Every variant id, in release order.
    pub fn variant_ids(&self) -> Vec<&str> {
        self.variants
            .iter()
            .map(|variant| variant.id.as_str())
            .collect()
    }

    /// The download a human would click, preferring the offline installer.
    pub fn human_download(&self) -> Option<&ReleaseDownload> {
        self.downloads
            .iter()
            .find(|download| download.kind == ReleaseDownloadKind::OfflineInstaller)
            .or_else(|| self.downloads.first())
    }

    /// The thin download, when the release publishes one.
    pub fn thin_download(&self) -> Option<&ReleaseDownload> {
        self.downloads
            .iter()
            .find(|download| download.kind == ReleaseDownloadKind::ThinInstaller)
    }

    /// The catalog descriptor an acquisition session schedules.
    pub fn catalog_descriptor(&self) -> ContentDescriptor {
        ContentDescriptor::of_document_kind(
            ContentKind::Catalog,
            self.catalog.digest,
            self.catalog.size,
        )
    }

    /// The descriptor for one variant's manifest.
    pub fn manifest_descriptor(&self, variant: &ReleaseVariant) -> ContentDescriptor {
        ContentDescriptor::of_document_kind(
            ContentKind::Metadata,
            variant.manifest.digest,
            variant.manifest.size,
        )
    }

    /// The descriptor for one variant's native runtime.
    pub fn runtime_descriptor(&self, variant: &ReleaseVariant) -> Option<ContentDescriptor> {
        variant.runtime.as_ref().map(|runtime| {
            ContentDescriptor::of_document_kind(ContentKind::Runtime, runtime.digest, runtime.size)
        })
    }
}

impl ContentDescriptor {
    /// Describe content that is carried and verified exactly as given.
    ///
    /// A document has one representation: its bytes on the wire are its
    /// logical bytes, so there is no second size and nothing to decompress.
    pub fn of_document_kind(kind: ContentKind, digest: Sha256Digest, size: u64) -> Self {
        Self::stored(kind, digest, size)
    }
}

/// The part of a release the fingerprint covers.
///
/// `release_digest` is excluded on purpose: a fingerprint cannot contain
/// itself. Everything a machine would act on is inside.
#[derive(Debug, Serialize)]
struct ReleaseBody<'a> {
    app_id: &'a AppId,
    channel: &'a str,
    version: &'a str,
    catalog: &'a DocumentRef,
    variants: &'a [ReleaseVariant],
    downloads: &'a [ReleaseDownload],
}

/// A bootstrapper's embedded trust configuration.
///
/// A thin artifact cannot be small unless it already knows where the release
/// graph lives and which root vouches for it, so it carries exactly this and
/// nothing else: no payload, no runtime, no other architecture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnlineTrust {
    pub app_id: AppId,
    pub channel: String,
    /// Repository root: the directory containing `metadata/` and `releases/`.
    pub repository: String,
    /// The TUF root every client starts from, base64-encoded `root.json` bytes.
    ///
    /// Base64 rather than a byte array because this document lives inside a
    /// JSON artifact index, where a byte array renders as one number per byte and
    /// triples the size of the one block a thin installer cannot avoid carrying.
    pub trusted_root: String,
    /// The release this bootstrapper is pinned to, or the channel it follows.
    pub pin: ReleasePin,
    /// Additional content origins, in preference order.
    ///
    /// This is a *hint*, not a trust grant: every blob is verified by digest
    /// wherever it comes from. It exists so an enterprise deployment can name
    /// its own mirror without a new signing system, and so the shape is in
    /// place for a signed mirror list later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    /// The installation scope this bootstrapper installs into.
    ///
    /// A thin artifact carries no variant manifest, so the scope the
    /// application's plan declares has nowhere else to travel. Without this the
    /// launcher picks one, and a manifest that says `machine` would be installed
    /// into the user's profile by an artifact nobody asked to be scoped that way.
    ///
    /// `either` is refused rather than encoded: a bootstrapper a person
    /// double-clicked cannot ask them a question, so a project that means `either`
    /// has to publish a thin artifact per scope or an offline one.
    #[serde(default)]
    pub scope: ThinScope,
}

/// The scope a thin bootstrapper installs into.
///
/// Its own type rather than a `String`, because the three values are not
/// interchangeable: `Either` is a *user choice* a bootstrapper cannot make, and
/// letting the string through would put the choice back as a silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinScope {
    /// The per-user profile. The default, and the only scope available without
    /// elevation.
    #[default]
    User,
    /// Every user on the machine. Requires elevation, which the runtime asks for
    /// after the bootstrapper has done its unelevated part.
    Machine,
}

impl ThinScope {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Machine => "machine",
        }
    }

    /// The scope this resolves to on a build host.
    pub const fn selected(self) -> zup_core::SelectedScope {
        match self {
            Self::User => zup_core::SelectedScope::User,
            Self::Machine => zup_core::SelectedScope::Machine,
        }
    }
}

impl OnlineTrust {
    /// Bound on the embedded trusted root.
    pub const MAX_ROOT_BYTES: u64 = 1024 * 1024;

    /// Bound on the number of named origins.
    pub const MAX_MIRRORS: usize = 8;

    /// Build a trust block from raw root bytes.
    pub fn new(
        app_id: AppId,
        channel: &str,
        repository: &str,
        trusted_root: &[u8],
        pin: ReleasePin,
    ) -> Self {
        Self {
            app_id,
            channel: channel.to_owned(),
            repository: repository.to_owned(),
            trusted_root: zup_core::base64_encode(trusted_root),
            pin,
            mirrors: Vec::new(),
            scope: ThinScope::User,
        }
    }

    /// State the scope this bootstrapper installs into.
    pub fn with_scope(mut self, scope: ThinScope) -> Self {
        self.scope = scope;
        self
    }

    /// The root bytes, decoded from the embedded base64.
    pub fn root_bytes(&self) -> Result<Vec<u8>, &'static str> {
        let bytes = zup_core::base64_decode(&self.trusted_root)
            .map_err(|_| "the embedded trusted root is not valid base64")?;
        if bytes.is_empty() {
            return Err("a trust configuration embeds no trusted root");
        }
        if bytes.len() as u64 > Self::MAX_ROOT_BYTES {
            return Err("an embedded trusted root exceeds 1 MiB");
        }
        Ok(bytes)
    }

    /// The digest that identifies the publisher's trust anchor.
    ///
    /// Two repositories can serve two graphs for one application id and one
    /// version. This is what an installed machine records so a later update or
    /// repair can tell them apart.
    pub fn trust_anchor(&self) -> Result<Sha256Digest, &'static str> {
        Ok(Sha256Digest::from_bytes(
            Sha256::digest(&self.root_bytes()?).into(),
        ))
    }

    /// Reject a trust configuration a hostile artifact could use to point a
    /// client somewhere unexpected.
    pub fn validate(&self) -> Result<(), &'static str> {
        check_channel(&self.channel)?;
        if self.repository.is_empty() {
            return Err("a trust configuration names no repository");
        }
        if self.repository.len() > 2048 {
            return Err("a repository URL is longer than 2048 bytes");
        }
        self.root_bytes()?;
        if self.mirrors.len() > Self::MAX_MIRRORS {
            return Err("a trust configuration names more origins than this build accepts");
        }
        match &self.pin {
            ReleasePin::Version { version } => {
                if version.is_empty() {
                    return Err("a pinned release names no version");
                }
            }
            ReleasePin::Channel { channel } => check_channel(channel)?,
        }
        Ok(())
    }

    /// The document this bootstrapper authenticates for its own release.
    ///
    /// A version-pinned bootstrapper addresses the immutable version-addressed
    /// document, so no later publication can change what it installs. A channel
    /// bootstrapper addresses the channel pointer, which is exactly the promise
    /// it makes and exactly as little.
    pub fn release_document(&self) -> Result<RelativeContentPath, &'static str> {
        match &self.pin {
            ReleasePin::Channel { .. } => crate::layout::release_path(&self.channel),
            ReleasePin::Version { version } => {
                crate::layout::release_version_path(&self.channel, version)
            }
        }
    }

    /// The version this bootstrapper insists on, when it is pinned.
    pub fn pinned_version(&self) -> Option<&str> {
        match &self.pin {
            ReleasePin::Version { version } => Some(version),
            ReleasePin::Channel { .. } => None,
        }
    }
}
