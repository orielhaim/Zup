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

const MAX_JSON_BYTES: u64 = 1 << 20;

const MAX_REDIRECTS: usize = 5;

pub struct GithubClient {
    host: GithubHost,
    client: HttpClient,
    api_origin: zup_acquire_http::Origin,
    upload_origin: zup_acquire_http::Origin,
    repository: String,
    max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientEndpoints {
    pub api: zup_acquire_http::Origin,
    pub upload: zup_acquire_http::Origin,
}

impl GithubClient {
    pub fn new(
        repository: &crate::repository::GithubRepository,
        token: &Token,
    ) -> Result<Self, GithubError> {
        Self::build(repository, token, None)
    }

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

    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    pub fn host(&self) -> &GithubHost {
        &self.host
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    fn api_url(&self, path: &str) -> Result<Url, GithubError> {
        let mut url = self.api_origin.url().clone();
        url.path_segments_mut()
            .map_err(|()| GithubError::Host {
                reason: "the API base is not a valid base URL".to_owned(),
            })?
            .extend(path.split('/').filter(|segment| !segment.is_empty()));
        Ok(url)
    }

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

    pub async fn release_by_id(&self, id: u64) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/{id}", self.repository))?;
        Release::from_json(self.get_json(&url).await?)
    }

    pub async fn latest_release(&self) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/latest", self.repository))?;
        Release::from_json(self.get_json(&url).await?)
    }

    pub async fn create_draft(&self, request: &CreateRelease) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases", self.repository))?;
        let body = serde_json::to_vec(request).map_err(|error| GithubError::Decode {
            reason: error.to_string(),
        })?;
        Release::from_json(self.send_json(&url, reqwest::Method::POST, body).await?)
    }

    pub async fn update(&self, id: u64, request: &CreateRelease) -> Result<Release, GithubError> {
        let url = self.api_url(&format!("repos/{}/releases/{id}", self.repository))?;
        let body = serde_json::to_vec(request).map_err(|error| GithubError::Decode {
            reason: error.to_string(),
        })?;
        Release::from_json(self.send_json(&url, reqwest::Method::PATCH, body).await?)
    }

    pub async fn publish(&self, id: u64, request: &CreateRelease) -> Result<Release, GithubError> {
        self.update(id, request).await
    }

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

fn is_retryable(response: &Response) -> bool {
    let status = response.status().as_u16();
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    status == 403
        && (response
            .header("x-ratelimit-remaining")
            .is_some_and(|value| value.trim() == "0")
            || response.header("retry-after").is_some())
}

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
    let message = response
        .read_to_end(64 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Json>(&body).ok())
        .and_then(|value| string_field(&value, "message"));
    if status == 404 {
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

fn describe_path(url: &Url) -> String {
    let mut out = url.path().to_owned();
    if let Some(host) = url.host_str() {
        out = format!("{host}{out}");
    }
    out
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub full_name: String,
    pub private: bool,
    pub archived: bool,
    pub default_branch: String,
    pub immutable_releases: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub immutable: Option<bool>,
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

    pub fn is_locked(&self) -> bool {
        self.immutable == Some(true) || !self.draft
    }

    pub fn url(&self) -> &str {
        &self.html_url
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub state: String,
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

    pub fn asset_state(&self) -> AssetState {
        if self.state.eq_ignore_ascii_case("starter") {
            AssetState::Starter
        } else {
            AssetState::Uploaded
        }
    }

    pub fn remote(&self) -> RemoteAsset {
        RemoteAsset {
            name: self.name.clone(),
            size: Some(self.size),
            digest: self.digest.as_deref().and_then(parse_digest),
            state: self.asset_state(),
        }
    }

    pub fn sha256(&self) -> Option<Sha256Digest> {
        self.digest.as_deref().and_then(parse_digest)
    }
}

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

pub use limits::LIMITS;
