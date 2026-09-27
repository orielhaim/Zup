//! The release plan: an immutable description of what is supposed to be released.
//!
//! A plan is built once, from a release manifest and whatever a future adapter
//! contributed, and every publisher consumes the same one. It is deliberately
//! *not* a build output: it says what a release is, and where a file sat on the
//! machine that produced it is not part of that. A publisher locates bytes on
//! its own and hands them over by product name.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::naming;

/// Version of the release plan shape.
///
/// Bumped when a field is added, removed, or retyped, so a plan written by one
/// version is never read as another.
pub const PLAN_SCHEMA: u32 = 1;

/// The application a release is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub id: zup_core::AppId,
    pub name: String,
    pub version: String,
}

impl Application {
    /// The application a release names.
    pub fn new(id: zup_core::AppId, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            version: version.into(),
        }
    }
}

/// What a release claims about the source it was built from.
///
/// The commit is a claim, not an assertion. A publisher compares it against what
/// the repository actually has, and `require_commit` is the switch that turns a
/// missing commit from a note into a refusal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClaim {
    /// The exact commit, when the build knew one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whether a release may be published without a commit.
    pub require_commit: bool,
}

impl SourceClaim {
    /// A claim with no commit and no insistence.
    pub fn none() -> Self {
        Self::default()
    }

    /// A claim about one commit.
    pub fn of(commit: impl Into<String>) -> Self {
        Self {
            commit: Some(commit.into()),
            require_commit: true,
        }
    }

    /// Whether this claim is satisfied.
    pub fn is_satisfied(&self) -> bool {
        self.commit.is_some() || !self.require_commit
    }
}

/// How a release's tag is spelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum TagPolicy {
    /// A prefix followed by the version. `v1.4.0` is the simple default, and the
    /// prefix is a value rather than a convention because a project that has
    /// shipped `release-1.4.0` cannot change it retroactively.
    Versioned {
        #[serde(default = "default_tag_prefix")]
        prefix: String,
    },
    /// A tag the project names outright.
    Exact { tag: String },
}

fn default_tag_prefix() -> String {
    "v".to_owned()
}

impl Default for TagPolicy {
    fn default() -> Self {
        Self::Versioned {
            prefix: default_tag_prefix(),
        }
    }
}

impl TagPolicy {
    /// The tag this policy spells for `version`.
    pub fn derive(&self, version: &str) -> Result<String, PlanError> {
        match self {
            Self::Versioned { prefix } => {
                if version.is_empty() {
                    return Err(PlanError::NoVersion);
                }
                if prefix.contains(|character: char| !naming::is_tag_prefix_byte(character as u8)) {
                    return Err(PlanError::TagPrefix {
                        prefix: prefix.clone(),
                    });
                }
                Ok(format!("{prefix}{version}"))
            }
            Self::Exact { tag } => {
                if tag.is_empty() {
                    return Err(PlanError::NoVersion);
                }
                Ok(tag.clone())
            }
        }
    }
}

/// The tag a release claims, and whether it may be created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagIntent {
    /// The tag, already derived.
    pub tag: String,
    /// How it was spelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<TagPolicy>,
    /// Whether a publisher may create the tag when it does not exist.
    ///
    /// Off by default. A provider that creates a tag from whichever commit its
    /// default branch happens to be pointing at will one day publish a release
    /// for code that was never built, and no amount of downstream verification
    /// puts that commit back.
    #[serde(default)]
    pub create: bool,
}

impl TagIntent {
    /// A tag that must already exist.
    pub fn required(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            policy: None,
            create: false,
        }
    }

    /// A tag a publisher may create.
    pub fn creatable(tag: impl Into<String>, policy: TagPolicy) -> Self {
        Self {
            tag: tag.into(),
            policy: Some(policy),
            create: true,
        }
    }
}

/// What a product is for.
///
/// A product's role is a claim about how it is consumed, which is why the same
/// file legitimately holds two roles: an install artifact and an update artifact
/// are the same `.exe` for a framework that reads its own installer, and two
/// different files for one that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductRole {
    /// A file a person downloads to install.
    Install,
    /// A file an updater fetches to become a later version.
    Update,
    /// Anything else a framework or a tool reads: update manifests, signatures,
    /// blockmaps, checksums.
    Auxiliary,
    /// A machine-readable description of the release or of part of it.
    Manifest,
}

impl ProductRole {
    /// Every role, in report order.
    pub const ALL: [ProductRole; 4] = [
        ProductRole::Install,
        ProductRole::Update,
        ProductRole::Auxiliary,
        ProductRole::Manifest,
    ];

    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Auxiliary => "auxiliary",
            Self::Manifest => "manifest",
        }
    }
}

/// Whether a person downloads this product as one file.
///
/// This is the difference between a limit that is a hard refusal and one that is
/// a sharding decision. A `Setup.exe` is one file or it is not the installer the
/// user asked for; splitting it hands them an archive with no name. A
/// transport package is an internal object the acquisition engine reads, so
/// splitting one changes nothing a human ever sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductClass {
    /// Downloaded and run by a person.
    UserFacing,
    /// Read by the acquisition engine, never by a person.
    Transport,
}

impl ProductClass {
    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserFacing => "user-facing",
            Self::Transport => "transport",
        }
    }

    /// Whether a host may split one of these to fit a per-file limit.
    pub const fn shardable(self) -> bool {
        matches!(self, Self::Transport)
    }
}

/// One file in a release, and every claim the release makes about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseProduct {
    /// The name this file carries in the release.
    ///
    /// Checked against a conservative character set at plan validation, because a
    /// provider is free to normalise a name it dislikes, and a release manifest
    /// that points at a filename the host quietly changed is worse than no
    /// release at all.
    pub name: String,
    /// What the file is for.
    pub role: ProductRole,
    /// Whether a person downloads it.
    pub class: ProductClass,
    /// A content type, when one applies. Free-form on purpose: this is
    /// information for a reader, not an input to any parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// The file's own digest, over its exact bytes.
    pub digest: Sha256Digest,
    /// The file's own size, in bytes.
    pub size: u64,
    /// The artifact or variant ids this file serves.
    ///
    /// Free-form, because a future adapter contributes things this crate cannot
    /// enumerate, and the publisher has to upload them without understanding
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serves: Vec<String>,
}

impl ReleaseProduct {
    /// A product with no media type and no served ids.
    pub fn new(
        name: impl Into<String>,
        role: ProductRole,
        class: ProductClass,
        digest: Sha256Digest,
        size: u64,
    ) -> Self {
        Self {
            name: name.into(),
            role,
            class,
            media_type: None,
            digest,
            size,
            serves: Vec::new(),
        }
    }

    /// Name this product serves `ids`.
    pub fn serving(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.serves.extend(ids);
        self.serves.sort();
        self.serves.dedup();
        self
    }

    /// Give this product a media type.
    pub fn with_media_type(mut self, media_type: impl Into<String>) -> Self {
        self.media_type = Some(media_type.into());
        self
    }
}

/// What kind of place a client fetches release content from.
///
/// The variants are deliberately coarse. The question a release plan answers is
/// "where do clients look", and a finer taxonomy would be this crate's opinion
/// about providers rather than a fact about the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    /// A plain static file tree.
    Static,
    /// Assets attached to a release, fetched by name.
    Release,
    /// A container registry.
    Container,
    /// A directory on this machine or a share.
    Local,
}

impl OriginKind {
    /// The name a report and a manifest use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Release => "release",
            Self::Container => "container",
            Self::Local => "local",
        }
    }
}

/// A place clients fetch this release's content from.
///
/// The plan records it so the release manifest can say where the thin bootstrapper
/// will look, and so a future migration has both answers side by side. It
/// records no identifier a provider owns, because a plan that named a release id
/// could not be used to plan a publication that has not happened yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentOrigin {
    pub kind: OriginKind,
    /// The base address, with a trailing slash.
    pub base: String,
    /// What this origin is for, for a report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ContentOrigin {
    /// An origin at `base`.
    pub fn new(kind: OriginKind, base: impl Into<String>) -> Self {
        Self {
            kind,
            base: base.into(),
            label: None,
        }
    }

    /// Name what this origin is for.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// What a host can hold.
///
/// Every number here is a host's own rule, so a plan is validated against the
/// host it is going to rather than against a constant baked into this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLimits {
    /// How many files one release may carry.
    pub max_assets: usize,
    /// The largest single file, in bytes. Exclusive: a file of exactly this size
    /// is already one too large.
    pub max_asset_bytes: u64,
}

impl HostLimits {
    /// A host that claims no limit.
    pub const UNLIMITED: Self = Self {
        max_assets: usize::MAX,
        max_asset_bytes: u64::MAX,
    };

    /// A host that accepts a product.
    pub const fn accepts_bytes(&self, size: u64) -> bool {
        size < self.max_asset_bytes
    }
}

impl Default for HostLimits {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// An immutable description of what a release consists of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePlan {
    pub schema: u32,
    pub application: Application,
    /// What the release claims about its source.
    #[serde(default)]
    pub source: SourceClaim,
    /// The tag the release is published under.
    pub tag: TagIntent,
    /// Files a person downloads to install.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_artifacts: Vec<ReleaseProduct>,
    /// Files an updater fetches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update_artifacts: Vec<ReleaseProduct>,
    /// Everything else a framework or tool reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_assets: Vec<ReleaseProduct>,
    /// Machine-readable descriptions of the release.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifests: Vec<ReleaseProduct>,
    /// Where clients fetch this release's content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_origins: Vec<ContentOrigin>,
}

impl ReleasePlan {
    /// An empty plan for an application.
    pub fn new(application: Application, tag: TagIntent) -> Self {
        Self {
            schema: PLAN_SCHEMA,
            application,
            source: SourceClaim::none(),
            tag,
            install_artifacts: Vec::new(),
            update_artifacts: Vec::new(),
            auxiliary_assets: Vec::new(),
            manifests: Vec::new(),
            content_origins: Vec::new(),
        }
    }

    /// The products of one role.
    pub fn role(&self, role: ProductRole) -> &[ReleaseProduct] {
        match role {
            ProductRole::Install => &self.install_artifacts,
            ProductRole::Update => &self.update_artifacts,
            ProductRole::Auxiliary => &self.auxiliary_assets,
            ProductRole::Manifest => &self.manifests,
        }
    }

    /// Add a product to its role's list.
    pub fn push(&mut self, product: ReleaseProduct) {
        self.role_mut(product.role).push(product);
    }

    /// The mutable list a role owns.
    pub fn role_mut(&mut self, role: ProductRole) -> &mut Vec<ReleaseProduct> {
        match role {
            ProductRole::Install => &mut self.install_artifacts,
            ProductRole::Update => &mut self.update_artifacts,
            ProductRole::Auxiliary => &mut self.auxiliary_assets,
            ProductRole::Manifest => &mut self.manifests,
        }
    }

    /// The claim about this release's source.
    pub fn with_source(mut self, source: SourceClaim) -> Self {
        self.source = source;
        self
    }

    /// Where clients fetch this release's content.
    pub fn with_origins(mut self, origins: Vec<ContentOrigin>) -> Self {
        self.content_origins = origins;
        self
    }

    /// Every distinct product, in name order.
    ///
    /// A file that holds two roles appears once, which is the point: an install
    /// artifact and an update artifact that are the same `.exe` is one upload.
    pub fn products(&self) -> Result<Vec<ReleaseProduct>, PlanError> {
        let mut merged: BTreeMap<String, ReleaseProduct> = BTreeMap::new();
        for role in ProductRole::ALL {
            for product in self.role(role) {
                match merged.get(&product.name) {
                    Some(existing) => {
                        if existing.digest != product.digest || existing.size != product.size {
                            return Err(PlanError::Incoherent {
                                name: product.name.clone(),
                            });
                        }
                    }
                    None => {
                        merged.insert(product.name.clone(), product.clone());
                    }
                }
            }
        }
        Ok(merged.into_values().collect())
    }

    /// The total bytes the release carries.
    pub fn bytes(&self) -> u64 {
        self.install_artifacts
            .iter()
            .chain(&self.update_artifacts)
            .chain(&self.auxiliary_assets)
            .chain(&self.manifests)
            .fold(0u64, |sum, product| sum.saturating_add(product.size))
    }

    /// The largest single file, with its name.
    pub fn largest(&self) -> Option<(&str, u64)> {
        self.install_artifacts
            .iter()
            .chain(&self.update_artifacts)
            .chain(&self.auxiliary_assets)
            .chain(&self.manifests)
            .fold(
                None,
                |largest: Option<(&str, u64)>, product| match largest {
                    Some((_, size)) if size >= product.size => largest,
                    _ => Some((product.name.as_str(), product.size)),
                },
            )
    }

    /// Whether the plan says anything at all.
    pub fn is_empty(&self) -> bool {
        ProductRole::ALL
            .iter()
            .all(|role| self.role(*role).is_empty())
    }

    /// Check the plan on its own terms, with no host in mind.
    pub fn validate(&self) -> Result<Vec<ReleaseProduct>, PlanError> {
        if self.schema != PLAN_SCHEMA {
            return Err(PlanError::Schema { found: self.schema });
        }
        if self.application.version.is_empty() {
            return Err(PlanError::NoVersion);
        }
        if self.tag.tag.is_empty() {
            return Err(PlanError::NoVersion);
        }
        let products = self.products()?;
        let mut seen = std::collections::BTreeSet::new();
        for product in &products {
            if !seen.insert(product.name.as_str()) {
                return Err(PlanError::Duplicate {
                    name: product.name.clone(),
                });
            }
            naming::check_asset_name(&product.name).map_err(|reason| PlanError::Name {
                name: product.name.clone(),
                reason,
            })?;
        }
        if !self.source.is_satisfied() {
            return Err(PlanError::NoCommit);
        }
        Ok(products)
    }

    /// Check the plan against a host, before any network mutation.
    ///
    /// This is the preflight. It runs on the products alone, so a release that
    /// cannot be published is refused before a draft exists, rather than after
    /// eleven gigabytes have been uploaded to one.
    pub fn preflight(&self, limits: &HostLimits) -> Result<Vec<ReleaseProduct>, PlanError> {
        let products = self.validate()?;
        if products.is_empty() {
            return Err(PlanError::Empty);
        }
        if products.len() > limits.max_assets {
            return Err(PlanError::TooManyAssets {
                count: products.len(),
                limit: limits.max_assets,
            });
        }
        for product in &products {
            if !limits.accepts_bytes(product.size) {
                return Err(PlanError::TooLarge {
                    name: product.name.clone(),
                    size: product.size,
                    limit: limits.max_asset_bytes,
                    splittable: product.class.shardable(),
                });
            }
        }
        Ok(products)
    }

    /// The plan as JSON.
    pub fn encode(&self) -> Result<Vec<u8>, PlanError> {
        serde_json::to_vec_pretty(self).map_err(|error| PlanError::Json(error.to_string()))
    }

    /// Read a plan, bounded, and validate it on its own terms.
    pub fn parse(bytes: &[u8]) -> Result<Self, PlanError> {
        let plan: Self =
            serde_json::from_slice(bytes).map_err(|error| PlanError::Json(error.to_string()))?;
        plan.validate()?;
        Ok(plan)
    }
}

impl fmt::Display for Application {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {} ({})", self.name, self.version, self.id)
    }
}

/// Why a release plan cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("the release plan is schema {found}; this build reads schema {PLAN_SCHEMA}")]
    Schema { found: u32 },
    #[error("the release names no version")]
    NoVersion,
    #[error(
        "a tag prefix may not contain `{prefix}`'s characters; a tag has to survive a filesystem and a URL"
    )]
    TagPrefix { prefix: String },
    #[error("two roles name `{name}` with different contents, which cannot be one file")]
    Incoherent { name: String },
    #[error("`{name}` appears more than once in the release plan")]
    Duplicate { name: String },
    #[error("`{name}` cannot be a release asset name: {reason}")]
    Name { name: String, reason: String },
    #[error("the release would need a commit, and none was recorded")]
    NoCommit,
    #[error("the release has no files to publish")]
    Empty,
    #[error("this host accepts {limit} assets per release, and the plan needs {count}")]
    TooManyAssets { count: usize, limit: usize },
    #[error(
        "`{name}` is {size} bytes, over this host's {limit} byte per-asset limit; \
         {}",
        if *splittable {
            "it is an internal transport object and may be published as shards"
        } else {
            "it is a file a person downloads, and splitting it would publish \
             something other than the installer that was signed"
        }
    )]
    TooLarge {
        name: String,
        size: u64,
        limit: u64,
        splittable: bool,
    },
    #[error("the release plan could not be read: {0}")]
    Json(String),
    /// A provider refused the plan for a reason that belongs to it.
    ///
    /// The provider's own sentence, kept whole. A release plan error is a
    /// statement about the plan, and a provider that has a better explanation
    /// than this crate could invent is the one that should give it.
    #[error("{0}")]
    Other(String),
}
