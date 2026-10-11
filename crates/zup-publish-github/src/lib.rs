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

pub const PROVIDER: &str = "github";

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Diagnosis {
    pub repository: String,
    pub host: String,
    pub private: bool,
    pub archived: bool,
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
    pub fn distribution_is_public(&self) -> bool {
        !self.private
    }
}
