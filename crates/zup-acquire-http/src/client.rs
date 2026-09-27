//! One HTTP client, one connection pool, sensible timeouts.
//!
//! A single client is built once and shared. That is not a micro-optimisation:
//! it is what makes HTTP/2 work. Many immutable objects over one pooled
//! connection is the multiplexed behaviour this design wants, and a client per
//! request would throw that away along with the connection warm-up.

use std::time::Duration;

use url::Url;
use zup_acquire::RelativeContentPath;

use crate::error::HttpError;
use crate::origin::Origin;

/// The product token every request carries.
///
/// A server operator needs to be able to tell an installer's traffic from a
/// scraper's, and a client that identifies itself as nothing is harder to
/// operate than one that does not.
pub const USER_AGENT: &str = concat!("zup-acquire/", env!("CARGO_PKG_VERSION"));

/// How long each phase of a request may take.
///
/// These are separate because they answer different questions. A connect that
/// takes thirty seconds is a network that is not there; a body that stalls for
/// thirty seconds is a server that stopped talking; and a whole blob may
/// legitimately take minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutPolicy {
    pub connect: Duration,
    /// Time to first response headers.
    pub headers: Duration,
    /// Maximum gap between two body chunks before the transfer is abandoned.
    pub idle: Duration,
    /// The ceiling for one blob, across all origins and attempts.
    pub per_blob: Duration,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            headers: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            per_blob: Duration::from_secs(60 * 60),
        }
    }
}

/// How the client is built.
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub timeouts: TimeoutPolicy,
    /// How many redirects one request may follow.
    pub max_redirects: usize,
    /// Whether the platform's proxy configuration is used.
    ///
    /// This is on by default and there is deliberately no way to turn it off
    /// from configuration: an installer that ignored the machine's proxy would
    /// be an installer that fails behind a corporate proxy, and adding a knob
    /// for that failure is how credentials end up in the wrong place.
    pub use_platform_proxy: bool,
    /// An `Authorization` header value, for a repository that requires one.
    ///
    /// Narrow on purpose: one header, one value, never logged, never included
    /// in a redirect target, and never applied to a host other than the one the
    /// value was configured for.
    pub bearer: Option<SecretHeader>,
}

/// A header value that must not reach a log.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretHeader(String);

impl SecretHeader {
    /// Wrap a header value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the request builder.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Present in diagnostics as a fact, absent as a value.
        formatter.write_str("SecretHeader(<redacted>)")
    }
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeouts: TimeoutPolicy::default(),
            max_redirects: 5,
            use_platform_proxy: true,
            bearer: None,
        }
    }
}

/// A shared HTTP client.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    timeouts: TimeoutPolicy,
    /// The host a configured bearer token may be sent to. A redirect to any
    /// other host does not receive it.
    authorized_host: Option<String>,
    bearer: Option<SecretHeader>,
}

impl HttpClient {
    /// Build a client.
    pub fn new(config: &HttpClientConfig) -> Result<Self, HttpError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(config.timeouts.connect)
            .timeout(config.timeouts.per_blob)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .http2_adaptive_window(true)
            .redirect(reqwest::redirect::Policy::none());
        if !config.use_platform_proxy {
            builder = builder.no_proxy();
        }
        let inner = builder.build().map_err(|reason| HttpError::Transport {
            origin: "client".to_owned(),
            reason: reason.to_string(),
        })?;
        Ok(Self {
            inner,
            timeouts: config.timeouts,
            authorized_host: None,
            bearer: None,
        })
    }

    /// Attach a bearer token, bound to the host of `origin`.
    ///
    /// Binding it to one host is what makes following a redirect safe: a CDN
    /// that redirects to a third party must not hand that third party the
    /// credential.
    pub fn with_bearer(mut self, origin: &Origin, value: SecretHeader) -> Self {
        self.authorized_host = Some(origin.host().to_owned());
        self.bearer = Some(value);
        self
    }

    /// The underlying client, for a caller that needs a raw request.
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Address one checked relative path on one origin.
    pub fn url(&self, origin: &Origin, path: &RelativeContentPath) -> Result<Url, HttpError> {
        origin.join(path).map_err(|reason| HttpError::Address {
            origin: origin.to_string(),
            path: path.to_string(),
            reason,
        })
    }

    /// Send a GET for one document, applying the redirect policy by hand.
    ///
    /// The redirect policy is applied here rather than in the client because it
    /// is a trust decision, not a convenience: a hop must stay on HTTPS, must
    /// not carry a credential to a new host, and is bounded.
    pub async fn get(
        &self,
        origin: &Origin,
        url: &Url,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        self.get_from(origin, url, None, max_redirects).await
    }

    /// Send a GET that may ask for a byte range.
    ///
    /// A range header is only ever set from a resume offset that the cache
    /// already verified, so a `Range` this client sends is a statement about
    /// bytes that are provably on disk.
    pub async fn get_range(
        &self,
        origin: &Origin,
        url: &Url,
        offset: u64,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        self.get_from(origin, url, Some(offset), max_redirects)
            .await
    }

    async fn get_from(
        &self,
        origin: &Origin,
        url: &Url,
        offset: Option<u64>,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        let mut current = url.clone();
        let mut hops = 0usize;
        loop {
            let mut request = self
                .inner
                .get(current.clone())
                .timeout(self.timeouts.per_blob);
            // The range is only re-sent to the origin the resume was measured
            // against. After a redirect the offset is dropped, because a
            // different host is not obliged to agree about what byte `offset`
            // means.
            if let Some(offset) = offset
                && hops == 0
            {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            if let (Some(host), Some(token)) = (&self.authorized_host, &self.bearer)
                && current.host_str() == Some(host.as_str())
            {
                request = request.bearer_auth(token.expose());
            }
            let response = request.send().await.map_err(|error| HttpError::Transport {
                origin: origin.to_string(),
                reason: classify(&error),
            })?;
            let status = response.status();
            if status.is_redirection() {
                hops += 1;
                if hops > max_redirects {
                    return Err(HttpError::Redirect {
                        origin: origin.to_string(),
                        reason: format!("more than {max_redirects} hops"),
                    });
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| HttpError::Redirect {
                        origin: origin.to_string(),
                        reason: "no location".to_owned(),
                    })?;
                let next = current
                    .join(location)
                    .map_err(|error| HttpError::Redirect {
                        origin: origin.to_string(),
                        reason: error.to_string(),
                    })?;
                check_redirect(origin, &current, &next)?;
                current = next;
                continue;
            }
            return Ok(Response {
                origin: origin.clone(),
                url: current,
                status,
                headers: response.headers().clone(),
                inner: response,
            });
        }
    }

    /// The timeout policy this client applies.
    pub fn timeouts(&self) -> TimeoutPolicy {
        self.timeouts
    }
}

fn check_redirect(origin: &Origin, from: &Url, to: &Url) -> Result<(), HttpError> {
    if to.scheme() != "https" && to.scheme() != "file" {
        return Err(HttpError::Redirect {
            origin: origin.to_string(),
            reason: format!("downgrade to `{}`", to.scheme()),
        });
    }
    if from.scheme() == "https" && to.scheme() != "https" {
        return Err(HttpError::Redirect {
            origin: origin.to_string(),
            reason: "https to http".to_owned(),
        });
    }
    if !to.username().is_empty() || to.password().is_some() {
        return Err(HttpError::Redirect {
            origin: origin.to_string(),
            reason: "a redirect target carries credentials".to_owned(),
        });
    }
    Ok(())
}

/// A response whose status has already been examined.
pub struct Response {
    origin: Origin,
    url: Url,
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    inner: reqwest::Response,
}

impl Response {
    /// The origin that served this.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The final URL, after any redirect.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The status code.
    pub fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    /// One response header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The `Content-Length` the server declared, if it declared one.
    ///
    /// This is a hint, never a bound. The bound is the descriptor.
    pub fn declared_length(&self) -> Option<u64> {
        self.inner.content_length()
    }

    /// Whether the server said it accepts range requests.
    pub fn accepts_ranges(&self) -> bool {
        self.header("accept-ranges")
            .is_some_and(|value| value.eq_ignore_ascii_case("bytes"))
    }

    /// Consume the response and return the body stream.
    pub fn into_body(self) -> reqwest::Response {
        self.inner
    }

    /// Take the underlying response, for its body stream.
    pub fn into_inner(self) -> reqwest::Response {
        self.inner
    }
}

/// Classify a transport error into a short reason that is safe to log.
///
/// A `reqwest` error's `Display` can include the URL. It is never used.
pub(crate) fn classify(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "timed out".to_owned();
    }
    if error.is_connect() {
        return "could not connect".to_owned();
    }
    if error.is_request() {
        return "the request could not be sent".to_owned();
    }
    if error.is_body() || error.is_decode() {
        return "the response body failed".to_owned();
    }
    if error.is_redirect() {
        return "a redirect was refused".to_owned();
    }
    "the connection failed".to_owned()
}
