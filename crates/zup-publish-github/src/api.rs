//! A small typed REST client for the release and release-asset endpoints.
//!
//! # Why not a GitHub SDK
//!
//! Because the surface is small and already enumerated: look up a repository, find a
//! release by tag, create a draft, list its assets, upload one, remove one, update the
//! release, generate notes. That is eight endpoints.
//!
//! Every general-purpose GitHub crate models the whole API and most bring a
//! version-locking mechanism that decides *which* response shapes are acceptable, which
//! is a poor fit for a client whose only job is to be exactly right about eight
//! endpoints. `octocrab` was measured against this surface and rejected: larger than
//! every crate that would use it combined, it types bodies behind generated enums that
//! do not round-trip a field GitHub adds, and it hides the retry and reconciliation
//! decisions that are the hard part of this problem behind a method name.
//!
//! Everything GitHub-shaped in zup is in this crate; the release model in
//! `zup-publish` has never heard of any of it.
//!
//! # What is enforced here rather than by a caller
//!
//! - **One API version, from one constant.** [`API_VERSION`](crate::endpoint::API_VERSION)
//!   is sent on every request, so no endpoint can be pinned to a different version by
//!   accident.
//! - **The credential goes to one host.** The token is bound to the API host it was
//!   configured for, and a redirect to a third party does not receive it.
//! - **A rate limit is not a permanent failure.** GitHub answers an exhausted limit with
//!   `403` or `429` plus `x-ratelimit-remaining: 0`, which a generic retry classifier
//!   reads as "do not retry". This client reads the headers.
//! - **An error message is read once.** GitHub's JSON error body is the only useful
//!   difference between "the tag is taken" and "the token is read-only", and a `422`
//!   with a field name in it is a different developer action from a `422` without one.

use std::time::Duration;

use serde_json::Value as Json;
use url::Url;
use zup_acquire_http::{
    Body, HttpClient, HttpClientConfig, HttpError, Request, Response, SecretHeader,
};
use zup_core::Sha256Digest;
use zup_publish::{AssetState, RemoteAsset};

use crate::endpoint::{
    ACCEPT_JSON, ACCEPT_UPLOAD, API_VERSION, CONTENT_TYPE_OCTET_STREAM, GithubHost,
};
use crate::error::GithubError;
use crate::limits;
use crate::publish::CreateRelease;
use crate::token::{Token, authorization};

/// The largest JSON body this client will read.
///
/// Every response it consumes is a release, a list of assets, or an error
/// message. A megabyte is three orders of magnitude more than any of those, and
/// a bound is what stops a misconfigured endpoint from being read into memory.
const MAX_JSON_BYTES: u64 = 1 << 20;

/// How many redirects one API request may follow.
///
/// GitHub's asset download URLs redirect, and its API does not. The bound is
/// small because a chain longer than this is not GitHub.
const MAX_REDIRECTS: usize = 5;

/// A GitHub REST client.
///
/// One client, one connection pool, and one place the policy lives. Constructing
/// one is cheap enough not to matter and expensive enough to do once, which is
/// why a publisher takes it rather than building it per request.
pub struct GithubClient {
    host: GithubHost,
    client: HttpClient,
    api_origin: zup_acquire_http::Origin,
    upload_origin: zup_acquire_http::Origin,
    repository: String,
    max_attempts: u32,
}

/// Where a client sends its requests, when that is not the installation's own
/// bases.
///
/// The default is the installation's `api_base` and `upload_base`, derived from
/// the repository's hostname. Two things need somewhere else:
///
/// - **A proxy.** An organisation that reaches GitHub through an egress proxy
///   addresses the proxy and lets it forward, rather than opening a direct
///   connection from a build runner.
/// - **A test.** The publisher's whole state machine is testable against a local
///   server, and a test hook that only exists in `cfg(test)` is a hook the
///   integration tests in `tests/` cannot reach.
///
/// Both are addresses, and both are still bound to one host for the credential,
/// so this cannot be used to send a token somewhere a redirect could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientEndpoints {
    pub api: zup_acquire_http::Origin,
    pub upload: zup_acquire_http::Origin,
}

impl GithubClient {
    /// Build a client for `repository`.
    ///
    /// The token is bound to the API host, which is what makes an Enterprise
    /// client's token unable to follow a redirect to github.com and vice versa.
    pub fn new(
        repository: &crate::repository::GithubRepository,
        token: &Token,
    ) -> Result<Self, GithubError> {
        Self::build(repository, token, None)
    }

    /// Build a client that addresses `endpoints`.
    pub fn at(
        repository: &crate::repository::GithubRepository,
        token: &Token,
        endpoints: &ClientEndpoints,
    ) -> Result<Self, GithubError> {
        Self::build(repository, token, Some(endpoints.clone()))
    }

    fn build(
        repository: &crate::repository::GithubRepository,
        token: &Token,
        endpoints: Option<ClientEndpoints>,
    ) -> Result<Self, GithubError> {
        let (api_origin, upload_origin) = match &endpoints {
            Some(endpoints) => (endpoints.api.clone(), endpoints.upload.clone()),
            None => (
                repository.host.api_origin()?,
                repository.host.upload_origin()?,
            ),
        };
        let client = HttpClient::new(&HttpClientConfig::default()).map_err(|error| {
            GithubError::Transport {
                reason: error.to_string(),
            }
        })?;
        Ok(Self {
            host: repository.host.clone(),
            client: client.with_bearer(&api_origin, SecretHeader::new(authorization(token))),
            api_origin,
            upload_origin,
            repository: repository.path(),
            max_attempts: 3,
        })
    }

    /// How many times one request is attempted.
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// The installation this client talks to.
    pub fn host(&self) -> &GithubHost {
        &self.host
    }

    /// The repository path this client addresses.
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Address an API path.
    fn api_url(&self, path: &str) -> Result<Url, GithubError> {
        let mut url = self.api_origin.url().clone();
        url.path_segments_mut()
            .map_err(|()| GithubError::Host {
                reason: "the API base is not a valid base URL".to_owned(),
            })?
            .extend(path.split('/').filter(|segment| !segment.is_empty()));
        Ok(url)
    }

    // ---------------------------------------------------------------- lookup

    /// Whether the repository exists and is reachable with this credential.
    ///
    /// Also the cheapest way to learn the visibility, which is the fact that
    /// decides whether a runtime client could fetch this release without a
    /// credential of its own.
    pub async fn repository_info(&self) -> Result<RepositoryInfo, GithubError> {
        let url = self.api_url(&format!("repos/{}", self.repository))?;
        let value = self.get_json(&url).await?;
        Ok(RepositoryInfo {
            full_name: string_field(&value, "full_name").unwrap_or_else(|| self.repository.clone()),
            private: value["private"].as_bool().unwrap_or(false),
            archived: value["archived"].as_bool().unwrap_or(false),
            default_branch: string_field(&value, "default_branch").unwrap_or_default(),
            immutable_releases: value["immutable_releases"].as_bool(),
        })
    }

    /// The release for `tag`, if one exists.
    pub async fn release_by_tag(&self, tag: &str) -> Result<Option<Release>, GithubError> {
        let url = self.api_url(&format!(
            "repos/{}/releases/tags/{}",
            self.repository,
            encode(tag)
        ))?;
        match self.get_json(&url).await {
            Ok(value) => Ok(Some(Release::from_json(value)?)),
            Err(GithubError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The release with `id`.
    pub async fn release_by_id(&self, id: u64) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/{id}", self.repository))?;
        Release::from_json(self.get_json(&url).await?)
    }

    /// The newest published release, which is what the `latest` alias means.
    pub async fn latest_release(&self) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/latest", self.repository))?;
        Release::from_json(self.get_json(&url).await?)
    }

    /// Create a release.
    ///
    /// The request asks for a draft unconditionally. A release that is created
    /// published and then filled with assets is a public release with holes in
    /// it, and no rerun of this tool can repair that.
    pub async fn create_draft(&self, request: &CreateRelease) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases", self.repository))?;
        let body = serde_json::to_vec(request).map_err(|error| GithubError::Decode {
            reason: error.to_string(),
        })?;
        Release::from_json(self.send_json(&url, reqwest::Method::POST, body).await?)
    }

    /// Update a release.
    pub async fn update(&self, id: u64, request: &CreateRelease) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/{id}", self.repository))?;
        let body = serde_json::to_vec(request).map_err(|error| GithubError::Decode {
            reason: error.to_string(),
        })?;
        Release::from_json(self.send_json(&url, reqwest::Method::PATCH, body).await?)
    }

    /// Publish a draft, or leave it a draft.
    ///
    /// `draft: false` is what makes a draft a release. Everything this tool does
    /// before this call is reversible; this call is the one that is not, which is
    /// why it is a single call at the end of a sequence that has already proved
    /// every file.
    pub async fn publish(&self, id: u64, request: &CreateRelease) -> Result<Release, GithubError> {
        self.update(id, request).await
    }

    /// GitHub's own generated release notes for a range.
    pub async fn generate_notes(
        &self,
        tag: &str,
        previous_tag: Option<&str>,
    ) -> Result<String, GithubError> {
        let url = self.api_url(&format!(
            "repos/{}/releases/generate-notes",
            self.repository
        ))?;
        let mut body = serde_json::Map::new();
        body.insert("tag_name".to_owned(), Json::String(tag.to_owned()));
        if let Some(previous) = previous_tag {
            body.insert(
                "previous_tag_name".to_owned(),
                Json::String(previous.to_owned()),
            );
        }
        let bytes = serde_json::to_vec(&body).map_err(|error| GithubError::Decode {
            reason: error.to_string(),
        })?;
        let value = self.send_json(&url, reqwest::Method::POST, bytes).await?;
        Ok(string_field(&value, "body").unwrap_or_default())
    }

    // ----------------------------------------------------------------- assets

    /// Every asset on a release.
    ///
    /// Paginated, because a release may carry a thousand of them and reading the
    /// first page of a list this code has to be complete about would be a bug
    /// that only appears on a large release.
    pub async fn assets(&self, release_id: u64) -> Result<Vec<Asset>, GithubError> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = self.api_url(&format!(
                "repos/{}/releases/{release_id}/assets?per_page=100&page={page}",
                self.repository
            ))?;
            let response = self.send(&url, reqwest::Method::GET, None).await?;
            let page_items: Vec<Json> = serde_json::from_slice(
                &response
                    .read_to_end(MAX_JSON_BYTES)
                    .await
                    .map_err(transport)?,
            )
            .map_err(|error| GithubError::Decode {
                reason: error.to_string(),
            })?;
            let count = page_items.len();
            all.extend(page_items.into_iter().filter_map(Asset::from_json));
            if count < 100 {
                return Ok(all);
            }
            page += 1;
        }
    }

    /// Upload one file as an asset.
    ///
    /// The body is a path, not a buffer. A 400 MiB installer that has to be
    /// resident to be uploaded is a limitation a developer discovers on the day
    /// they ship, and the whole point of a release publisher is that a large
    /// release is not a special occasion.
    ///
    /// `content_type` matters here and is not decorative. GitHub treats the
    /// upload endpoint's `Content-Type` as the asset's type, and the API version
    /// media type has to be present on this endpoint in particular; an upload
    /// without it is how an asset ends up in the `starter` state.
    pub async fn upload_asset(
        &self,
        release_id: u64,
        name: &str,
        path: &std::path::Path,
        size: u64,
    ) -> Result<Asset, GithubError> {
        let mut url = self.upload_origin.url().clone();
        url.path_segments_mut()
            .map_err(|()| GithubError::Host {
                reason: "the upload base is not a valid base URL".to_owned(),
            })?
            .extend(
                format!("repos/{}/releases/{release_id}/assets", self.repository)
                    .split('/')
                    .filter(|segment| !segment.is_empty()),
            );
        url.query_pairs_mut().append_pair("name", name);
        let declared = std::fs::metadata(path)
            .map(|meta| meta.len())
            .map_err(|error| GithubError::Io {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?;
        if declared != size {
            // The size came from a digest-and-measure pass. If the file changed
            // underneath it, everything downstream - the cache, the attestation,
            // the plan - is describing bytes that no longer exist, so this is
            // refused rather than uploaded.
            return Err(GithubError::Size {
                name: name.to_owned(),
                expected: size,
                found: declared,
            });
        }
        let body = Body::File {
            path: path.to_path_buf(),
            chunk: Body::UPLOAD_CHUNK,
        };
        let request = Request::post(&url, body)
            .header("Accept", ACCEPT_UPLOAD)
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Content-Type", CONTENT_TYPE_OCTET_STREAM);
        let response = self.exchange_once(request).await?;
        if !response.status().is_success() {
            return Err(status_error(response, &url).await);
        }
        let value = read_json(response).await?;
        Asset::from_json(value).ok_or_else(|| GithubError::Decode {
            reason: format!("the upload of `{name}` returned no asset"),
        })
    }

    /// Remove one asset.
    ///
    /// The only asset removal this client performs is a `starter` remnant of a
    /// failed upload on a draft. It exists because a host that recorded the name
    /// before the bytes arrived will refuse every subsequent upload of that name,
    /// and the alternative is a release carrying a zero-byte installer.
    pub async fn delete_asset(&self, asset_id: u64) -> Result<(), GithubError> {
        let url = self.api_url(&format!(
            "repos/{}/releases/assets/{asset_id}",
            self.repository
        ))?;
        let response = self.exchange(Request::delete(&url)).await?;
        if response.status() == reqwest::StatusCode::NO_CONTENT || response.status().is_success() {
            return Ok(());
        }
        Err(status_error(response, &url).await)
    }

    async fn get_json(&self, url: &Url) -> Result<Json, GithubError> {
        let request = Request::get(url).header("Accept", ACCEPT_JSON);
        let response = self.exchange(request).await?;
        if !response.status().is_success() {
            return Err(status_error(response, url).await);
        }
        read_json(response).await
    }

    async fn send_json(
        &self,
        url: &Url,
        method: reqwest::Method,
        body: Vec<u8>,
    ) -> Result<Json, GithubError> {
        let request = Request::new(method, url)
            .header("Accept", ACCEPT_JSON)
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .with_body(body);
        let response = self.exchange(request).await?;
        if !response.status().is_success() {
            return Err(status_error(response, url).await);
        }
        read_json(response).await
    }

    async fn send(
        &self,
        url: &Url,
        method: reqwest::Method,
        body: Option<Vec<u8>>,
    ) -> Result<Response, GithubError> {
        let mut request = Request::new(method, url)
            .header("Accept", ACCEPT_JSON)
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(body) = body {
            request = request
                .with_body(body)
                .header("Content-Type", "application/json");
        }
        let response = self.exchange(request).await?;
        if !response.status().is_success() {
            return Err(status_error(response, url).await);
        }
        Ok(response)
    }

    /// One request, retried on a bounded policy.
    ///
    /// The retry decision belongs here rather than in the generic HTTP retry
    /// classifier because two of GitHub's failure signals are not statuses: an
    /// exhausted rate limit is a `403`, and a secondary limit is a `429` with no
    /// `Retry-After`. A client that reads only the status code turns a two-second
    /// wait into a failed release.
    ///
    /// Asset uploads are the one exception, and deliberately so. A retry of an
    /// upload restarts the whole transfer, and GitHub's documented failure mode
    /// for a failed upload leaves a `starter` asset that takes the name hostage -
    /// so a retry is only safe after the host has been asked what it now holds.
    /// That reconciliation belongs to the publisher, which is the only layer that
    /// can do it, and it cannot happen here.
    async fn exchange(&self, request: Request<'_>) -> Result<Response, GithubError> {
        self.try_exchange(request, true).await
    }

    async fn exchange_once(&self, request: Request<'_>) -> Result<Response, GithubError> {
        self.try_exchange(request, false).await
    }

    async fn try_exchange(
        &self,
        request: Request<'_>,
        retry: bool,
    ) -> Result<Response, GithubError> {
        let mut attempt = 1u32;
        let mut delay = Duration::from_millis(500);
        loop {
            let response = self
                .client
                .send(&self.api_origin, request.clone(), MAX_REDIRECTS)
                .await
                .map_err(transport)?;
            let status = response.status();
            if status.is_success()
                || !retry
                || !is_retryable(&response)
                || attempt >= self.max_attempts
            {
                return Ok(response);
            }
            let wait = response
                .header("retry-after")
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| {
                    response
                        .header("x-ratelimit-reset")
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(|epoch| Duration::from_secs(epoch.saturating_sub(now_seconds())))
                        .unwrap_or(delay)
                        .min(Duration::from_secs(60))
                });
            // The response has to be consumed before the connection is reused,
            // and a body that is not read is a body that holds the connection.
            drop(response);
            tokio::time::sleep(wait.min(Duration::from_secs(60))).await;
            delay = (delay * 2).min(Duration::from_secs(30));
            attempt += 1;
        }
    }
}

async fn read_json(response: Response) -> Result<Json, GithubError> {
    let body = response
        .read_to_end(MAX_JSON_BYTES)
        .await
        .map_err(transport)?;
    serde_json::from_slice(&body).map_err(|error| GithubError::Decode {
        reason: error.to_string(),
    })
}

fn transport(error: HttpError) -> GithubError {
    GithubError::Transport {
        reason: error.reason(),
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Whether a response is worth sending again.
///
/// GitHub's own rate-limit signals, plus the statuses that are genuinely
/// transient. A `422` is not here: a duplicate filename is a `422`, and
/// retrying a duplicate filename produces a second `422` and a slower release.
fn is_retryable(response: &Response) -> bool {
    let status = response.status().as_u16();
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    // A 403 is a rate limit when the headers say so, and a permission failure
    // when they do not. Only the first is worth waiting out.
    status == 403
        && (response
            .header("x-ratelimit-remaining")
            .is_some_and(|value| value.trim() == "0")
            || response.header("retry-after").is_some())
}

/// Turn a failed response into the error a developer should read.
async fn status_error(response: Response, url: &Url) -> GithubError {
    let status = response.status().as_u16();
    if matches!(status, 403 | 429)
        && response
            .header("x-ratelimit-remaining")
            .is_some_and(|value| value.trim() == "0")
    {
        return GithubError::RateLimited {
            seconds: response
                .header("retry-after")
                .and_then(|value| value.trim().parse::<u64>().ok())
                .or_else(|| {
                    response
                        .header("x-ratelimit-reset")
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(|epoch| epoch.saturating_sub(now_seconds()))
                }),
        };
    }
    // GitHub's error body is the only useful difference between "the tag is
    // taken" and "the token is read-only", so it is read before the error is
    // built. Bounded, because an error body is a sentence and not a payload.
    let message = response
        .read_to_end(64 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Json>(&body).ok())
        .and_then(|value| string_field(&value, "message"));
    if status == 404 {
        // The API deliberately does not distinguish "no such release" from "no
        // such repository" from "your token cannot see it", so a caller that
        // needs to know why has to ask the repository endpoint itself. This
        // client preserves the ambiguity rather than guessing, and says so.
        return GithubError::NotFound {
            what: format!("{status} for {}", describe_path(url)),
        };
    }
    if status == 401 {
        return GithubError::Unauthenticated {
            reason: message.unwrap_or_else(|| "the token was rejected".to_owned()),
        };
    }
    GithubError::status(status, message)
}

/// The path of a URL, with no query, for a diagnostic.
///
/// A query string on a release-asset download is a signed redirect target, and
/// a diagnostic is not the place that belongs.
fn describe_path(url: &Url) -> String {
    let mut out = url.path().to_owned();
    if let Some(host) = url.host_str() {
        out = format!("{host}{out}");
    }
    out
}
/// A repository, as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub full_name: String,
    /// Whether the repository is private.
    ///
    /// The fact a runtime client needs: a private release's assets need a
    /// credential a thin installer cannot carry, so this decides whether GitHub
    /// can be the content host for a project at all.
    pub private: bool,
    pub archived: bool,
    pub default_branch: String,
    /// Whether immutable releases are enabled, when the API says.
    ///
    /// `Option` because it is `None` on a GitHub Enterprise Server too old to
    /// have the field, and "not reported" is not "not enabled".
    pub immutable_releases: Option<bool>,
}

/// A release, as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    /// Whether the release is immutable, when the API says.
    pub immutable: Option<bool>,
    /// Whether GitHub holds a release attestation for it.
    pub has_attestation: Option<bool>,
    pub html_url: String,
    pub target_commitish: Option<String>,
}

impl Release {
    fn from_json(value: Json) -> Result<Self, GithubError> {
        let id = value["id"].as_u64().ok_or_else(|| GithubError::Decode {
            reason: "a release has no id".to_owned(),
        })?;
        let immutable = value.get("immutable").and_then(Json::as_bool);
        // GitHub creates the release attestation when a release is published
        // immutably, and there is no separate field for it. Its presence is
        // therefore what a caller can check, and `None` is the honest answer on
        // a server that reports neither rather than a claim that it does not
        // exist.
        let has_attestation = value
            .get("immutable")
            .or_else(|| value.get("attestations"))
            .and_then(Json::as_bool);
        Ok(Self {
            id,
            tag_name: string_field(&value, "tag_name").unwrap_or_default(),
            name: string_field(&value, "name"),
            body: string_field(&value, "body"),
            draft: value["draft"].as_bool().unwrap_or(false),
            prerelease: value["prerelease"].as_bool().unwrap_or(false),
            immutable,
            has_attestation,
            html_url: string_field(&value, "html_url").unwrap_or_default(),
            target_commitish: string_field(&value, "target_commitish"),
        })
    }

    /// Whether this release will not accept further mutation.
    pub fn is_locked(&self) -> bool {
        self.immutable == Some(true) || !self.draft
    }

    /// The browser address of this release.
    pub fn url(&self) -> &str {
        &self.html_url
    }
}

/// A release asset, as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub state: String,
    /// The digest GitHub computed, in `sha256:<hex>` form.
    pub digest: Option<String>,
    pub browser_download_url: String,
}

impl Asset {
    fn from_json(value: Json) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()?,
            name: string_field(&value, "name")?,
            size: value.get("size").and_then(Json::as_u64).unwrap_or(0),
            state: string_field(&value, "state").unwrap_or_else(|| "uploaded".to_owned()),
            digest: string_field(&value, "digest"),
            browser_download_url: string_field(&value, "browser_download_url").unwrap_or_default(),
        })
    }

    /// The asset's own state, in the release plane's terms.
    pub fn asset_state(&self) -> AssetState {
        if self.state.eq_ignore_ascii_case("starter") {
            AssetState::Starter
        } else {
            AssetState::Uploaded
        }
    }

    /// The asset as the release plane describes it.
    ///
    /// A digest the API reports but that is not a `sha256:` is discarded rather
    /// than passed on: `classify` compares SHA-256, and a digest of some other
    /// algorithm that was coerced into this field would compare equal to nothing
    /// and be treated as an unverifiable conflict, which is a worse outcome than
    /// an honest "this host does not tell us".
    pub fn remote(&self) -> RemoteAsset {
        RemoteAsset {
            name: self.name.clone(),
            size: Some(self.size),
            digest: self.digest.as_deref().and_then(parse_digest),
            state: self.asset_state(),
        }
    }

    /// The asset's SHA-256, as the API reports it.
    pub fn sha256(&self) -> Option<Sha256Digest> {
        self.digest.as_deref().and_then(parse_digest)
    }
}

/// Parse the `digest` field, which is `sha256:<hex>`.
pub fn parse_digest(value: &str) -> Option<Sha256Digest> {
    let hex = value.strip_prefix("sha256:")?;
    hex.parse().ok()
}

fn string_field(value: &Json, name: &str) -> Option<String> {
    match value.get(name)? {
        Json::String(text) if !text.is_empty() => Some(text.clone()),
        Json::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Percent-encode one path segment.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The limits this provider enforces, re-exported so a caller does not have to
/// reach for a second crate to state one.
pub use limits::LIMITS;
