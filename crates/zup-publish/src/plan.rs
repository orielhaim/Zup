use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::naming;

pub const PLAN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub id: zup_core::AppId,
    pub name: String,
    pub version: String,
}

impl Application {
    pub fn new(id: zup_core::AppId, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            version: version.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClaim {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub require_commit: bool,
}

impl SourceClaim {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn of(commit: impl Into<String>) -> Self {
        Self {
            commit: Some(commit.into()),
            require_commit: true,
        }
    }

    pub fn is_satisfied(&self) -> bool {
        match &self.commit {
            Some(commit) => valid_commit(commit),
            None => !self.require_commit,
        }
    }
}

pub fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag != "@"
        && !tag.starts_with('-')
        && !tag.contains(['\\', ' ', '~', '^', ':', '?', '*', '['])
        && !tag.bytes().any(|b| b.is_ascii_control() || b == 127)
        && !tag.contains("..")
        && !tag.contains("@{")
        && tag.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum TagPolicy {
    Versioned {
        #[serde(default = "default_tag_prefix")]
        prefix: String,
    },
    Exact {
        tag: String,
    },
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagIntent {
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<TagPolicy>,
    #[serde(default)]
    pub create: bool,
}

impl TagIntent {
    pub fn required(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            policy: None,
            create: false,
        }
    }

    pub fn creatable(tag: impl Into<String>, policy: TagPolicy) -> Self {
        Self {
            tag: tag.into(),
            policy: Some(policy),
            create: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductRole {
    Install,
    Update,
    Auxiliary,
    Manifest,
}

impl ProductRole {
    pub const ALL: [ProductRole; 4] = [
        ProductRole::Install,
        ProductRole::Update,
        ProductRole::Auxiliary,
        ProductRole::Manifest,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Auxiliary => "auxiliary",
            Self::Manifest => "manifest",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductClass {
    UserFacing,
    Transport,
    Metadata,
    NativeImage,
}

impl ProductClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserFacing => "user-facing",
            Self::Transport => "transport",
            Self::Metadata => "metadata",
            Self::NativeImage => "native-image",
        }
    }

    pub const fn shardable(self) -> bool {
        matches!(self, Self::Transport)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseProduct {
    pub name: String,
    pub role: ProductRole,
    pub class: ProductClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub digest: Sha256Digest,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serves: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
}

impl ReleaseProduct {
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
            requires: Vec::new(),
        }
    }

    pub fn serving(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.serves.extend(ids);
        self.serves.sort();
        self.serves.dedup();
        self
    }

    pub fn with_media_type(mut self, media_type: impl Into<String>) -> Self {
        self.media_type = Some(media_type.into());
        self
    }

    pub fn requiring(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.requires.extend(names);
        self.requires.sort();
        self.requires.dedup();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    Static,
    Release,
    Container,
    Local,
}

impl OriginKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Release => "release",
            Self::Container => "container",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentOrigin {
    pub kind: OriginKind,
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ContentOrigin {
    pub fn new(kind: OriginKind, base: impl Into<String>) -> Self {
        Self {
            kind,
            base: base.into(),
            label: None,
        }
    }

    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLimits {
    pub max_assets: usize,
    pub max_asset_bytes: u64,
}

impl HostLimits {
    pub const UNLIMITED: Self = Self {
        max_assets: usize::MAX,
        max_asset_bytes: u64::MAX,
    };

    pub const fn accepts_bytes(&self, size: u64) -> bool {
        size < self.max_asset_bytes
    }
}

impl Default for HostLimits {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePlan {
    pub schema: u32,
    pub application: Application,
    #[serde(default)]
    pub source: SourceClaim,
    pub tag: TagIntent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_artifacts: Vec<ReleaseProduct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update_artifacts: Vec<ReleaseProduct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_assets: Vec<ReleaseProduct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifests: Vec<ReleaseProduct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_origins: Vec<ContentOrigin>,
}

impl ReleasePlan {
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

    pub fn role(&self, role: ProductRole) -> &[ReleaseProduct] {
        match role {
            ProductRole::Install => &self.install_artifacts,
            ProductRole::Update => &self.update_artifacts,
            ProductRole::Auxiliary => &self.auxiliary_assets,
            ProductRole::Manifest => &self.manifests,
        }
    }

    pub fn push(&mut self, product: ReleaseProduct) {
        self.role_mut(product.role).push(product);
    }

    pub fn role_mut(&mut self, role: ProductRole) -> &mut Vec<ReleaseProduct> {
        match role {
            ProductRole::Install => &mut self.install_artifacts,
            ProductRole::Update => &mut self.update_artifacts,
            ProductRole::Auxiliary => &mut self.auxiliary_assets,
            ProductRole::Manifest => &mut self.manifests,
        }
    }

    pub fn with_source(mut self, source: SourceClaim) -> Self {
        self.source = source;
        self
    }

    pub fn with_origins(mut self, origins: Vec<ContentOrigin>) -> Self {
        self.content_origins = origins;
        self
    }

    pub fn products(&self) -> Result<Vec<ReleaseProduct>, PlanError> {
        let mut merged: BTreeMap<String, ReleaseProduct> = BTreeMap::new();
        for role in ProductRole::ALL {
            for product in self.role(role) {
                match merged.get_mut(&product.name) {
                    Some(existing) => {
                        if existing.digest != product.digest
                            || existing.size != product.size
                            || existing.class != product.class
                            || existing.media_type != product.media_type
                        {
                            return Err(PlanError::Incoherent {
                                name: product.name.clone(),
                            });
                        }
                        existing.requires.extend(product.requires.iter().cloned());
                        existing.requires.sort();
                        existing.requires.dedup();
                        existing.serves.extend(product.serves.iter().cloned());
                        existing.serves.sort();
                        existing.serves.dedup();
                    }
                    None => {
                        merged.insert(product.name.clone(), product.clone());
                    }
                }
            }
        }
        Ok(merged.into_values().collect())
    }

    pub fn bytes(&self) -> u64 {
        let mut seen = std::collections::BTreeSet::new();
        self.install_artifacts
            .iter()
            .chain(&self.update_artifacts)
            .chain(&self.auxiliary_assets)
            .chain(&self.manifests)
            .filter(|product| seen.insert(&product.name))
            .fold(0u64, |sum, product| sum.saturating_add(product.size))
    }

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

    pub fn is_empty(&self) -> bool {
        ProductRole::ALL
            .iter()
            .all(|role| self.role(*role).is_empty())
    }

    pub fn validate(&self) -> Result<Vec<ReleaseProduct>, PlanError> {
        if self.schema != PLAN_SCHEMA {
            return Err(PlanError::Schema { found: self.schema });
        }
        if self.application.version.is_empty() {
            return Err(PlanError::NoVersion);
        }
        if !valid_tag(&self.tag.tag) {
            return Err(PlanError::Other(
                "invalid Git tag; use a complete refs/tags name without ref syntax".to_owned(),
            ));
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
        for product in &products {
            for required in &product.requires {
                if !seen.contains(required.as_str()) {
                    return Err(PlanError::MissingDependency {
                        name: product.name.clone(),
                        required: required.clone(),
                    });
                }
            }
        }
        let mut complete = std::collections::BTreeSet::new();
        while complete.len() < products.len() {
            let before = complete.len();
            for product in &products {
                if product
                    .requires
                    .iter()
                    .all(|name| complete.contains(name.as_str()))
                {
                    complete.insert(product.name.as_str());
                }
            }
            if complete.len() == before {
                return Err(PlanError::DependencyCycle);
            }
        }
        if !self.source.is_satisfied() {
            return Err(PlanError::NoCommit);
        }
        Ok(products)
    }

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

    pub fn encode(&self) -> Result<Vec<u8>, PlanError> {
        serde_json::to_vec_pretty(self).map_err(|error| PlanError::Json(error.to_string()))
    }

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
    #[error(
        "the release requires a full lowercase 40-hex commit SHA; record the build source and rebuild"
    )]
    NoCommit,
    #[error("`{name}` requires missing release asset `{required}`")]
    MissingDependency { name: String, required: String },
    #[error("release assets contain a dependency cycle")]
    DependencyCycle,
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
            "it must be uploaded intact to preserve its declared download contract"
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
    #[error("{0}")]
    Other(String),
}
