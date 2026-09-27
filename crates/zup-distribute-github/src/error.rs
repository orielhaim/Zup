//! Why a GitHub release could not be read as content.

/// Why a transport package could not be built, opened, or trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackageError {
    #[error("the package is schema {found}; this build reads schema {schema}")]
    Schema { found: u32, schema: u32 },
    #[error(
        "the package requires feature bits {required:#x}; this build understands {supported:#x}"
    )]
    Features { required: u64, supported: u64 },
    #[error("this is not a GitHub transport package")]
    Magic,
    #[error("the package is truncated: {found} bytes, expected at least {expected}")]
    Short { expected: u64, found: u64 },
    #[error("the package metadata is {found} bytes, over the {limit} byte limit")]
    Metadata { limit: u64, found: u64 },
    #[error("the package metadata hashes to sha256:{found}; the header says sha256:{expected}")]
    MetadataDigest { expected: String, found: String },
    #[error("the package metadata could not be read: {0}")]
    Json(String),
    #[error("the package's `{0}` is empty")]
    Field(&'static str),
    #[error("the package's {0} are not in the order its format requires")]
    Order(&'static str),
    #[error("a frame at byte {offset} falls outside every shard")]
    Unmapped { offset: u64 },
    #[error("the package's metadata length did not settle; the container is not writable")]
    UnstableMetadata,
    #[error("a blob appears twice with different bytes")]
    FrameConflict { digest: zup_core::Sha256Digest },
    #[error(
        "a frame compressed to something that would decompress to {found} bytes, over the {limit} byte limit"
    )]
    Expansion { limit: u64, found: u64 },
    #[error("a frame decompressed to sha256:{found}; the release named sha256:{expected}")]
    BlobDigest { expected: String, found: String },
    #[error("a frame could not be decompressed: {0}")]
    Compression(String),
    #[error("{0}")]
    Io(String),
}

impl From<DistributeError> for PackageError {
    fn from(error: DistributeError) -> Self {
        Self::Io(error.to_string())
    }
}
/// Why a release could not be read as content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DistributeError {
    /// The repository or release could not be named.
    #[error("github release: {0}")]
    Repository(String),

    /// A document was fetched but does not mean what it should.
    #[error("`{name}` does not match what the release authenticated: {reason}")]
    Untrusted { name: String, reason: String },

    /// A release asset could not be fetched.
    #[error("`{name}`: {reason}")]
    Fetch { name: String, reason: String },

    /// The release exists but cannot serve content to a thin installer.
    #[error(
        "`{repository}` is a private repository, so its release assets need a credential; a \
         bootstrapper cannot carry one. Publish the release from a public repository, or host \
         the content on an origin that serves anonymous reads."
    )]
    Private { repository: String },

    /// The variant the host needs is not in the release.
    #[error("the release has no package for this machine; it carries {available}")]
    NoPackage { available: String },

    #[error("{0}")]
    Package(#[from] PackageError),
}

impl From<std::string::String> for DistributeError {
    fn from(reason: std::string::String) -> Self {
        Self::Repository(reason)
    }
}

impl From<zup_publish_github::GithubError> for DistributeError {
    fn from(error: zup_publish_github::GithubError) -> Self {
        Self::Repository(error.to_string())
    }
}

impl DistributeError {
    /// A fetch failure, for a caller that has a name and a reason.
    pub fn fetch(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Fetch {
            name: name.into(),
            reason: reason.into(),
        }
    }
}
