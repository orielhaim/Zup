//! Where a release's files are, and which release.
//!
//! # Two release references, and why they are different things
//!
//! ```text
//! Pinned   https://github.com/owner/repo/releases/download/v1.4.0/<asset>
//! Latest   https://github.com/owner/repo/releases/latest/download/<asset>
//! ```
//!
//! A pinned reference is an identity: it names one immutable release, and it
//! resolves to the same bytes forever. A thin installer built for a version must
//! use it, because a bootstrapper that can silently become a later release is a
//! bootstrapper that will.
//!
//! The `latest` reference is a *channel*, and it is the one place zup uses
//! GitHub's own opinion about which release is newest. That is a defensible
//! choice for a conventional "stable" channel and nothing more. It is not a
//! channel system: it cannot express `beta`, `nightly`, or `canary`, and zup does
//! not pretend otherwise. Complex channels stay on the generic TUF and
//! content-origin path, where a channel is a signed pointer rather than a
//! redirect.
//!
//! # The redirect is transport, never trust
//!
//! `releases/latest/download/...` is a redirect. It is followed, current
//! redirects and all, and the stable release URL is what zup records as a
//! source's identity. A signed URL with an expiry in it is a fetch target for
//! this transfer and nothing else — caching one as an address would make a
//! temporary credential into a permanent identity.
//!
//! Nothing about the redirect decides what is installed. The release a client
//! ends up trusting is authenticated by the release descriptor and TUF metadata
//! it fetches and verifies, exactly as it is on a static origin.

use url::Url;
use zup_publish_github::{GithubHost, GithubRepository};

/// Which release's assets to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseRef {
    /// One exact tag.
    Tag(String),
    /// Whatever the host currently calls latest.
    Latest,
}

impl ReleaseRef {
    /// An exact tag.
    pub fn tag(tag: impl Into<String>) -> Self {
        Self::Tag(tag.into())
    }

    /// The stable channel.
    pub fn latest() -> Self {
        Self::Latest
    }

    /// The name a report uses.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Tag(tag) => tag,
            Self::Latest => "latest",
        }
    }

    /// Whether this reference always resolves to the same release.
    pub const fn is_pinned(&self) -> bool {
        matches!(self, Self::Tag(_))
    }
}

/// A release, and where its assets live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseLayout {
    pub repository: GithubRepository,
    pub release: ReleaseRef,
}

impl ReleaseLayout {
    /// A layout for one release.
    pub fn new(repository: GithubRepository, release: ReleaseRef) -> Self {
        Self {
            repository,
            release,
        }
    }

    /// A layout for one exact tag.
    pub fn pinned(repository: GithubRepository, tag: impl Into<String>) -> Self {
        Self::new(repository, ReleaseRef::Tag(tag.into()))
    }

    /// A layout for the stable channel.
    pub fn latest(repository: GithubRepository) -> Self {
        Self::new(repository, ReleaseRef::Latest)
    }

    /// The stable address of one asset.
    ///
    /// The *stable* address, always: the permanent form addressed by tag where
    /// there is a tag, and the `latest` alias where there is not. Never the
    /// signed URL a redirect resolves to.
    pub fn asset_url(&self, asset: &str) -> Result<Url, zup_publish_github::GithubError> {
        let path = self.repository.path();
        let raw = match &self.release {
            ReleaseRef::Tag(tag) => self.repository.host.download_url(&path, tag, asset),
            ReleaseRef::Latest => self.repository.host.latest_download_url(&path, asset),
        };
        Url::parse(&raw).map_err(|error| zup_publish_github::GithubError::Host {
            reason: format!("{raw} is not a URL: {error}"),
        })
    }

    /// The browser address of the release.
    pub fn release_url(&self) -> Option<String> {
        match &self.release {
            ReleaseRef::Tag(tag) => Some(
                self.repository
                    .host
                    .release_url(&self.repository.path(), tag),
            ),
            ReleaseRef::Latest => None,
        }
    }

    /// The installation this layout can serve.
    pub fn host(&self) -> &GithubHost {
        &self.repository.host
    }
}
