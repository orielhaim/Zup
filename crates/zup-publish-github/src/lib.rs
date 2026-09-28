//! GitHub Releases as a zup release provider.
//!
//! # What this crate is
//!
//! Everything zup knows about GitHub. The release model in `zup-publish` has no idea
//! this crate exists, and neither does `zup-core`, `zup-plan`, `zup-runtime`,
//! `zup-transaction`, or the artifact semantics. GitHub is a *release provider*, and a
//! provider is one implementation of a plane that another provider could also
//! implement.
//!
//! ```text
//! zup-publish          ReleasePlan, HostLimits, classify(), PublishReceipt
//!        │
//!        ├─ zup-publish-github        this crate: publish a release to GitHub
//!        └─ (a GitLab provider)       the same plan, a different host
//!
//! zup-distribute-github  read release content back out of GitHub
//! ```
//!
//! The split from `zup-distribute-github` is the build plane / content plane
//! distinction, and it is a crate boundary because the dependency only runs one way:
//! the content source reads the release plane's identity model (a host, a repository,
//! an address) and the release plane never learns what a content package is.
//!
//! # What a publication does
//!
//! ```text
//! resolve/verify tag
//!     ↓
//! create or find draft          resumable
//!     ↓
//! upload every required asset   resumable; a matching asset is skipped
//!     ↓
//! verify every remote asset     name, size, SHA-256, state
//!     ↓
//! publish the draft once
//! ```
//!
//! The draft is the point. A draft is a resumable publication: eleven of twelve files
//! uploaded is a state a rerun can finish, not a state to throw away. And the release
//! becomes public in exactly one call, after every file has been proved against the
//! digest the local build computed.
//!
//! # Credentials
//!
//! A token is read from `GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token`. It is
//! never written to `zup.toml`, never embedded in a release, never printed, and never
//! stored in a receipt. A token that reached a committed manifest would reach every
//! fork of the project, which is why the manifest has no field for one and the type
//! that holds a token stores it in a [`secrecy::SecretString`] - `Debug` prints
//! `[REDACTED]`, `Display` does not exist, and the value is only reachable through an
//! `expose_secret` call that reads like the dangerous thing it is.
//!
//! # Enterprise
//!
//! [`endpoint::GithubHost`] models the web base, the API base, and the upload base as
//! one value with a hostname in it. github.com is a default; `GITHUB_API_URL` and
//! `GITHUB_SERVER_URL` are honoured when they are set, which is what makes an
//! Enterprise installation work without a code change. Features a given server version
//! may not have - immutable releases above all - are feature-detected and reported as
//! "not reported" rather than as "not enabled".

#![forbid(unsafe_code)]

mod api;
mod config;
mod endpoint;
mod error;
mod limits;
mod notes;
mod pins;
mod publish;
mod receipt;
mod repository;
mod runner;
mod token;
mod workflow;

pub use api::{Asset, ClientEndpoints, GithubClient, Release, RepositoryInfo, parse_digest};
pub use config::{PublishConfig, content_origin};
pub use endpoint::{
    ACCEPT_JSON, ACCEPT_UPLOAD, API_VERSION, CONTENT_TYPE_OCTET_STREAM, GITHUB_COM, GithubHost,
};
pub use error::GithubError;
pub use limits::{LIMITS, MAX_ASSET_BYTES, MAX_ASSETS, PACKAGE_SHARD_BYTES, accepts, limit_text};
pub use notes::compose as notes_compose;
pub use notes::{DownloadRow, NotesPolicy, download_section};
pub use pins::{
    ActionPin, LOCK_PATH, LOCK_SCHEMA, LockedAction, PinError, PinLock, generated_actions,
    infrastructure_actions, lock, lock_json, pin, pins,
};
pub use publish::{CreateRelease, PublishRequest, compose_notes, publish};
pub use receipt::{GithubAsset, GithubReceipt, RECEIPT_SCHEMA};
pub use repository::{
    Discovery, Environment, GithubRepository, ProcessEnvironment, RepositorySpec, Resolved,
    find_git_config, parse_remote_url, parse_remotes, read_git_config, resolve,
    resolve_from_remotes,
};
pub use runner::{
    Runner, WINDOWS_ARM, WINDOWS_ARM_VS2026, cross_runner, is_windows, native_runner,
};
pub use token::{Token, authorization, discover, discover_with, supplied};
pub use workflow::{
    Freshness, MatrixTarget, Signing, WORKFLOW_PATH, WorkflowPolicy, check, generate,
};

/// The provider name a report and a receipt use.
pub const PROVIDER: &str = "github";

/// A diagnostic for one publication, without performing one.
///
/// `zup doctor` needs to answer "would this work" without creating a draft, and
/// the answer is the same preflight the publisher runs, plus the two facts only a
/// live repository can supply: whether the credential works and whether the
/// repository is private.
pub async fn diagnose(
    repository: &GithubRepository,
    token: &Token,
    plan: &zup_publish::ReleasePlan,
) -> Result<Diagnosis, GithubError> {
    let products = plan.preflight(&LIMITS)?;
    let client = GithubClient::new(repository, token)?;
    let info = client.repository_info().await?;
    let existing = client.release_by_tag(&plan.tag.tag).await?;
    Ok(Diagnosis {
        repository: repository.to_string(),
        host: repository.host.host.clone(),
        private: info.private,
        archived: info.archived,
        immutable_releases: info.immutable_releases,
        tag: plan.tag.tag.clone(),
        assets: products.len(),
        asset_limit: MAX_ASSETS,
        largest: plan.largest().map(|(name, size)| (name.to_owned(), size)),
        asset_bytes_limit: MAX_ASSET_BYTES,
        release_exists: existing.is_some(),
        release_is_draft: existing
            .as_ref()
            .map(|release| release.draft)
            .unwrap_or(false),
        release_is_immutable: existing.as_ref().and_then(|release| release.immutable),
    })
}

/// What `zup doctor` reports about a GitHub publication.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Diagnosis {
    pub repository: String,
    pub host: String,
    pub private: bool,
    pub archived: bool,
    /// Whether immutable releases are enabled, when the host says.
    pub immutable_releases: Option<bool>,
    pub tag: String,
    pub assets: usize,
    pub asset_limit: usize,
    pub largest: Option<(String, u64)>,
    pub asset_bytes_limit: u64,
    pub release_exists: bool,
    pub release_is_draft: bool,
    pub release_is_immutable: Option<bool>,
}

impl Diagnosis {
    /// Whether GitHub can serve this release's content to a thin installer.
    ///
    /// Deliberately narrow and deliberately about the *host* rather than the
    /// repository's current visibility: a private repository's assets need a
    /// credential, and a thin installer cannot carry one. An archived repository is
    /// reported separately because it can be un-archived.
    pub fn distribution_is_public(&self) -> bool {
        !self.private
    }
}
