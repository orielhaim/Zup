use std::fmt;

use zup_acquire_http::Origin;

use crate::error::GithubError;

pub const GITHUB_COM: &str = "github.com";

pub const API_GITHUB_COM: &str = "api.github.com";

pub const UPLOADS_GITHUB_COM: &str = "uploads.github.com";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubHost {
    pub host: String,
    pub api_base: String,
    pub upload_base: String,
    pub web_base: String,
    pub dotcom: bool,
}

impl GithubHost {
    pub fn dotcom() -> Self {
        Self {
            host: GITHUB_COM.to_owned(),
            api_base: format!("https://{API_GITHUB_COM}/"),
            upload_base: format!("https://{UPLOADS_GITHUB_COM}/"),
            web_base: format!("https://{GITHUB_COM}/"),
            dotcom: true,
        }
    }

    pub fn enterprise(host: &str) -> Result<Self, GithubError> {
        let host = host.trim().trim_end_matches('/').to_ascii_lowercase();
        if host.is_empty() {
            return Err(GithubError::Host {
                reason: "a GitHub Enterprise hostname cannot be empty".to_owned(),
            });
        }
        if host == GITHUB_COM || host == API_GITHUB_COM || host == UPLOADS_GITHUB_COM {
            return Ok(Self::dotcom());
        }
        if host.contains('/') || host.contains('@') || host.contains(' ') {
            return Err(GithubError::Host {
                reason: format!("`{host}` is not a hostname"),
            });
        }
        Ok(Self {
            web_base: format!("https://{host}/"),
            api_base: format!("https://{host}/api/v3/"),
            upload_base: format!("https://{host}/uploads/"),
            host,
            dotcom: false,
        })
    }

    pub fn api_host(&self) -> &str {
        self.api_base
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.host)
    }

    pub fn upload_host(&self) -> &str {
        self.upload_base
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.host)
    }

    pub fn api_origin(&self) -> Result<Origin, GithubError> {
        Origin::parse(&self.api_base).map_err(|reason| GithubError::Host { reason })
    }

    pub fn upload_origin(&self) -> Result<Origin, GithubError> {
        Origin::parse(&self.upload_base).map_err(|reason| GithubError::Host { reason })
    }

    pub fn download_url(&self, repository: &str, tag: &str, asset: &str) -> String {
        format!(
            "{}{}/releases/download/{}/{}",
            self.web_base,
            repository,
            encode_segment(tag),
            encode_segment(asset)
        )
    }

    pub fn latest_download_url(&self, repository: &str, asset: &str) -> String {
        format!(
            "{}{}/releases/latest/download/{}",
            self.web_base,
            repository,
            encode_segment(asset)
        )
    }

    pub fn release_url(&self, repository: &str, tag: &str) -> String {
        format!(
            "{}{}/releases/tag/{}",
            self.web_base,
            repository,
            encode_segment(tag)
        )
    }

    pub fn repository_path(&self, repository: &str) -> String {
        format!("repos/{}", repository.trim_matches('/'))
    }
}

impl fmt::Display for GithubHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.web_base)
    }
}

fn encode_segment(value: &str) -> String {
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

pub const API_VERSION: &str = "2022-11-28";

pub const ACCEPT_JSON: &str = "application/vnd.github+json";

pub const ACCEPT_UPLOAD: &str = "application/vnd.github+json";

pub const CONTENT_TYPE_OCTET_STREAM: &str = "application/octet-stream";
