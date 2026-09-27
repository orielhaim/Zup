//! A GitHub installation: one host, three base addresses.
//!
//! GitHub.com is a default, not an assumption. An Enterprise installation has
//! the same three bases under a different hostname, and a code path that spells
//! `api.github.com` in a format string is a code path that cannot be pointed at
//! one. So the endpoints live here, in one type, and the hostname is a value
//! that discovery sets and everything downstream reads.
//!
//! The three bases are genuinely different services:
//!
//! | base      | github.com                              | Enterprise                         |
//! |-----------|-----------------------------------------|------------------------------------|
//! | web       | `https://github.com/`                   | `https://<host>/`                   |
//! | api       | `https://api.github.com/`               | `https://<host>/api/v3/`            |
//! | upload    | `https://uploads.github.com/`           | `https://<host>/uploads/`           |
//!
//! Uploads are a separate service with a separate rate limit, which is why an
//! implementation that shares one base with the API cannot read the upload
//! budget off an API response.

use std::fmt;

use zup_acquire_http::Origin;

use crate::error::GithubError;

/// The hostname GitHub.com is served from.
pub const GITHUB_COM: &str = "github.com";

/// The hostname GitHub's API is served from on github.com.
pub const API_GITHUB_COM: &str = "api.github.com";

/// The hostname GitHub's asset upload service is served from on github.com.
pub const UPLOADS_GITHUB_COM: &str = "uploads.github.com";

/// One GitHub installation's addresses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubHost {
    /// The web hostname, e.g. `github.com` or `git.acme.internal`.
    pub host: String,
    /// The REST API base, with a trailing slash.
    pub api_base: String,
    /// The asset upload base, with a trailing slash.
    pub upload_base: String,
    /// The web base, with a trailing slash.
    pub web_base: String,
    /// Whether this is github.com rather than an Enterprise installation.
    pub dotcom: bool,
}

impl GithubHost {
    /// github.com.
    pub fn dotcom() -> Self {
        Self {
            host: GITHUB_COM.to_owned(),
            api_base: format!("https://{API_GITHUB_COM}/"),
            upload_base: format!("https://{UPLOADS_GITHUB_COM}/"),
            web_base: format!("https://{GITHUB_COM}/"),
            dotcom: true,
        }
    }

    /// An Enterprise installation at `host`.
    ///
    /// Enterprise serves its API under `/api/v3` and its upload service under
    /// `/uploads` on the same hostname, which is the part that differs from
    /// github.com and the reason the two cannot be the same string with a
    /// variable substituted into it.
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

    /// The host that serves the API, which is not the web host on github.com.
    pub fn api_host(&self) -> &str {
        self.api_base
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.host)
    }

    /// The host that serves uploads.
    pub fn upload_host(&self) -> &str {
        self.upload_base
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.host)
    }

    /// The API base as an origin, for the client to address.
    pub fn api_origin(&self) -> Result<Origin, GithubError> {
        Origin::parse(&self.api_base).map_err(|reason| GithubError::Host { reason })
    }

    /// The upload base as an origin.
    ///
    /// A local test server is not HTTPS, and the origin type is right to refuse
    /// one. A caller that needs to point the client somewhere else passes the
    /// origin it built rather than asking this to launder it.
    pub fn upload_origin(&self) -> Result<Origin, GithubError> {
        Origin::parse(&self.upload_base).map_err(|reason| GithubError::Host { reason })
    }

    /// The stable download address of one asset in a release.
    ///
    /// The permanent form, addressed by tag, is what a document should name. The
    /// `latest` alias exists too, but it is a *redirect to whatever is newest*,
    /// so it is a channel rather than an identity: a name that pointed at it
    /// would resolve to a different file next month.
    pub fn download_url(&self, repository: &str, tag: &str, asset: &str) -> String {
        format!(
            "{}{}/releases/download/{}/{}",
            self.web_base,
            repository,
            encode_segment(tag),
            encode_segment(asset)
        )
    }

    /// The address that always resolves to the newest non-draft, non-prerelease
    /// release's asset.
    pub fn latest_download_url(&self, repository: &str, asset: &str) -> String {
        format!(
            "{}{}/releases/latest/download/{}",
            self.web_base,
            repository,
            encode_segment(asset)
        )
    }

    /// The browser address of a release.
    pub fn release_url(&self, repository: &str, tag: &str) -> String {
        format!(
            "{}{}/releases/tag/{}",
            self.web_base,
            repository,
            encode_segment(tag)
        )
    }

    /// The REST API path prefix for `repositories/{owner}/{name}`.
    pub fn repository_path(&self, repository: &str) -> String {
        format!("repos/{}", repository.trim_matches('/'))
    }
}

impl fmt::Display for GithubHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.web_base)
    }
}

/// Percent-encode one path segment.
///
/// A tag or an asset name goes into a URL path, and `zup` already restricts
/// generated names to a safe set, but a tag is often something a person typed.
/// Encoding it here means a tag with a space in it is a 404 rather than a path
/// that means something else.
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

/// The GitHub REST API version every request declares.
///
/// One place, and it is a constant rather than a configuration value: a client
/// that negotiates its own API version against a host is a client whose behaviour
/// changes with a repository setting.
pub const API_VERSION: &str = "2022-11-28";

/// The `Accept` header a JSON request carries.
pub const ACCEPT_JSON: &str = "application/vnd.github+json";

/// The `Accept` header an asset upload carries.
///
/// The API version media type is required for the upload endpoint specifically;
/// omitting it is how an upload ends up as a `starter` asset with no error.
pub const ACCEPT_UPLOAD: &str = "application/vnd.github+json";

/// The `Content-Type` an asset upload carries.
pub const CONTENT_TYPE_OCTET_STREAM: &str = "application/octet-stream";
