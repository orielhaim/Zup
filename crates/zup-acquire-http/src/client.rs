//! One HTTP client, one connection pool, sensible timeouts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use futures_util::StreamExt;
use url::Url;
use zup_acquire::RelativeContentPath;

use crate::error::HttpError;
use crate::origin::Origin;

/// The product token every request carries.
pub const USER_AGENT: &str = concat!("zup-acquire/", env!("CARGO_PKG_VERSION"));

/// How long each phase of a request may take.
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
    pub total: usize,
    pub per_origin: usize,
    /// Whether the platform's proxy configuration is used.
    pub use_platform_proxy: bool,
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
        formatter.write_str("SecretHeader(<redacted>)")
    }
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeouts: TimeoutPolicy::default(),
            total: 12,
            per_origin: 6,
            use_platform_proxy: true,
        }
    }
}

/// A shared HTTP client.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    limits: Arc<Limits>,
    timeouts: TimeoutPolicy,
    credentials: Vec<(url::Origin, SecretHeader)>,
}

struct Limits {
    total: Arc<Semaphore>,
    origins: Mutex<HashMap<url::Origin, Weak<Semaphore>>>,
    per_origin: usize,
}

impl HttpClient {
    /// Build a client.
    pub fn new(config: &HttpClientConfig) -> Result<Self, HttpError> {
        let policy = config.timeouts;
        if config.total == 0
            || config.per_origin == 0
            || [policy.connect, policy.headers, policy.idle, policy.per_blob]
                .iter()
                .any(Duration::is_zero)
            || policy.connect > policy.per_blob
            || policy.headers > policy.per_blob
            || policy.idle > policy.per_blob
        {
            return Err(HttpError::Transport {
                origin: "client".into(),
                reason: "invalid HTTP limits or timeout policy".into(),
            });
        }
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(config.timeouts.connect)
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
            limits: Arc::new(Limits {
                total: Arc::new(Semaphore::new(config.total)),
                origins: Mutex::new(HashMap::new()),
                per_origin: config.per_origin,
            }),
            timeouts: config.timeouts,
            credentials: Vec::new(),
        })
    }

    /// Authorize an exact scheme, host, and port. Redirects do not expand this grant.
    pub fn with_bearer(mut self, origin: &Origin, value: SecretHeader) -> Self {
        let origin = origin.url().origin();
        self.credentials.retain(|(existing, _)| existing != &origin);
        self.credentials.push((origin, value));
        self
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
    pub async fn get(
        &self,
        origin: &Origin,
        url: &Url,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        self.get_from(origin, url, None, max_redirects).await
    }

    /// Send a GET that may ask for a byte range.
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
        let request = match offset {
            Some(offset) => Request::get(url).range(Range::from(offset)),
            None => Request::get(url),
        };
        self.exchange(origin, request, max_redirects)
            .await
            .map_err(|error| match (error, offset) {
                (HttpError::Redirect { origin, .. }, Some(_)) => {
                    HttpError::RangeUnsupported { origin }
                }
                (error, _) => error,
            })
    }

    /// The timeout policy this client applies.
    pub fn timeouts(&self) -> TimeoutPolicy {
        self.timeouts
    }

    /// Send one request, applying the redirect and credential policy by hand.
    pub async fn send(
        &self,
        origin: &Origin,
        request: Request<'_>,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        self.exchange(origin, request, max_redirects).await
    }
}

impl HttpClient {
    /// The one place a request is put on the wire, for every caller.
    async fn exchange(
        &self,
        origin: &Origin,
        request: Request<'_>,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        let deadline = tokio::time::Instant::now() + self.timeouts.per_blob;
        let mut current = request.url.clone();
        let mut hops = 0usize;
        loop {
            let origin_limit = {
                let mut origins = self.limits.origins.lock().expect("origin limits poisoned");
                origins.retain(|_, limit| limit.strong_count() > 0);
                let entry = origins.entry(current.origin()).or_default();
                match entry.upgrade() {
                    Some(limit) => limit,
                    None => {
                        let limit = Arc::new(Semaphore::new(self.limits.per_origin));
                        *entry = Arc::downgrade(&limit);
                        limit
                    }
                }
            };
            let permits = tokio::time::timeout_at(deadline, async {
                let origin = origin_limit
                    .acquire_owned()
                    .await
                    .expect("origin semaphore open");
                let total = self
                    .limits
                    .total
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("total semaphore open");
                (origin, total)
            })
            .await
            .map_err(|_| HttpError::Timeout { phase: "capacity" })?;
            let mut builder = self
                .inner
                .request(request.method.clone(), current.clone())
                .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()));
            for (name, value) in &request.headers {
                if current.origin() != request.url.origin()
                    && ["authorization", "proxy-authorization", "cookie"]
                        .iter()
                        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
                {
                    continue;
                }
                builder = builder.header(*name, value.clone());
            }
            if let Some(range) = request.range {
                builder = builder.header(reqwest::header::RANGE, range.header());
            }
            if let Some((_, token)) = self
                .credentials
                .iter()
                .find(|(origin, _)| *origin == current.origin())
            {
                builder = builder.bearer_auth(token.expose());
            }
            if let Some(body) = &request.body {
                match body {
                    Body::Bytes(bytes) => builder = builder.body(bytes.clone()),
                    Body::File(path) => {
                        let file = tokio::fs::File::open(path).await.map_err(|error| {
                            HttpError::Transport {
                                origin: "file".to_owned(),
                                reason: error.to_string(),
                            }
                        })?;
                        builder = builder.body(file);
                    }
                }
            }
            let headers_deadline =
                deadline.min(tokio::time::Instant::now() + self.timeouts.headers);
            let response = tokio::time::timeout_at(headers_deadline, builder.send())
                .await
                .map_err(|_| HttpError::Timeout { phase: "headers" })?
                .map_err(|error| transport_error(origin.to_string(), &error))?;
            let status = response.status();
            if status.is_redirection() {
                // Replaying a mutation or resuming another origin's representation is unsafe.
                if request.body.is_some()
                    || request.range.is_some()
                    || (request.method != reqwest::Method::GET
                        && request.method != reqwest::Method::HEAD)
                {
                    return Err(HttpError::Redirect {
                        origin: origin.to_string(),
                        reason: "only complete, bodyless reads may be redirected".to_owned(),
                    });
                }
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
                inner: response,
                permits,
                deadline,
                idle: self.timeouts.idle,
            });
        }
    }
}

/// A byte range to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    /// The last byte, inclusive. `None` asks for everything from `start`.
    pub end: Option<u64>,
}

impl Range {
    /// Everything from `start` onwards.
    pub const fn from(start: u64) -> Self {
        Self { start, end: None }
    }

    /// `start..=end`.
    pub const fn between(start: u64, end: u64) -> Self {
        Self {
            start,
            end: Some(end),
        }
    }

    /// The `Range` header value.
    pub fn header(&self) -> String {
        match self.end {
            Some(end) => format!("bytes={}-{end}", self.start),
            None => format!("bytes={}-", self.start),
        }
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub enum Body {
    /// Bytes the caller already has.
    Bytes(Vec<u8>),
    /// A file streamed from disk.
    File(std::path::PathBuf),
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        Self::Bytes(value.as_bytes().to_vec())
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Self::Bytes(value.into_bytes())
    }
}

impl From<Vec<u8>> for Body {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

/// One request, in the parts the policy does not decide.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    method: reqwest::Method,
    url: &'a Url,
    headers: Vec<(&'a str, String)>,
    body: Option<Body>,
    range: Option<Range>,
}

impl<'a> Request<'a> {
    /// A request with no headers, no body, and no range.
    pub fn new(method: reqwest::Method, url: &'a Url) -> Self {
        Self {
            method,
            url,
            headers: Vec::new(),
            body: None,
            range: None,
        }
    }

    /// A `GET`.
    pub fn get(url: &'a Url) -> Self {
        Self::new(reqwest::Method::GET, url)
    }

    /// A `POST` carrying `body`.
    pub fn post(url: &'a Url, body: impl Into<Body>) -> Self {
        Self::new(reqwest::Method::POST, url).with_body(body)
    }

    /// A `DELETE`.
    pub fn delete(url: &'a Url) -> Self {
        Self::new(reqwest::Method::DELETE, url)
    }

    /// Attach a body.
    pub fn with_body(mut self, body: impl Into<Body>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Add a header.
    pub fn header(mut self, name: &'a str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Ask for a byte range.
    pub fn range(mut self, range: Range) -> Self {
        self.range = Some(range);
        self
    }
}

fn check_redirect(origin: &Origin, from: &Url, to: &Url) -> Result<(), HttpError> {
    if to.scheme() != "https" {
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
    inner: reqwest::Response,
    permits: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    deadline: tokio::time::Instant,
    idle: Duration,
}

impl Response {
    /// The origin that served this.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The final URL, after any redirect.
    pub fn url(&self) -> &Url {
        self.inner.url()
    }

    /// The status code.
    pub fn status(&self) -> reqwest::StatusCode {
        self.inner.status()
    }

    /// One response header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    /// The `Content-Length` the server declared, if it declared one.
    pub fn declared_length(&self) -> Option<u64> {
        self.inner.content_length()
    }

    /// Whether the server said it accepts range requests.
    pub fn accepts_ranges(&self) -> bool {
        self.header("accept-ranges")
            .is_some_and(|value| value.eq_ignore_ascii_case("bytes"))
    }

    /// Consume the response, retaining request permits until the stream is dropped.
    pub fn bytes_stream(
        self,
    ) -> futures_util::stream::BoxStream<'static, Result<bytes::Bytes, HttpError>> {
        let stream = self.inner.bytes_stream().boxed();
        let origin = self.origin.to_string();
        futures_util::stream::unfold(
            Some((stream, self.permits, self.deadline, self.idle, origin)),
            |state| async move {
                let (mut stream, permits, deadline, idle, origin) = state?;
                let result = tokio::time::timeout_at(
                    deadline.min(tokio::time::Instant::now() + idle),
                    stream.next(),
                )
                .await;
                let chunk = match result {
                    Ok(Some(chunk)) => {
                        chunk.map_err(|error| transport_error(origin.clone(), &error))
                    }
                    Ok(None) => return None,
                    Err(_) => Err(HttpError::Timeout {
                        phase: if tokio::time::Instant::now() >= deadline {
                            "blob"
                        } else {
                            "idle"
                        },
                    }),
                };
                let next = if chunk.is_ok() {
                    Some((stream, permits, deadline, idle, origin))
                } else {
                    None
                };
                Some((chunk, next))
            },
        )
        .boxed()
    }

    pub async fn bytes(self) -> Result<Vec<u8>, HttpError> {
        let mut body = Vec::new();
        let mut stream = self.bytes_stream();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk?);
        }
        Ok(body)
    }

    /// Read the whole body, bounded by `limit`.
    pub async fn read_to_end(self, limit: u64) -> Result<Vec<u8>, HttpError> {
        let mut body = Vec::new();
        self.stream_into(limit, |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        })
        .await?;
        Ok(body)
    }

    /// Stream the body into `sink`, one chunk at a time.
    pub async fn stream_into<S>(self, limit: u64, mut sink: S) -> Result<u64, HttpError>
    where
        S: FnMut(&[u8]) -> std::io::Result<()>,
    {
        if self.declared_length().is_some_and(|length| length > limit) {
            return Err(HttpError::TooLarge {
                origin: self.origin.to_string(),
                limit,
            });
        }
        let origin = self.origin.clone();
        let mut stream = std::pin::pin!(self.bytes_stream());
        let mut produced = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            produced = produced.saturating_add(chunk.len() as u64);
            if produced > limit {
                return Err(HttpError::TooLarge {
                    origin: origin.to_string(),
                    limit,
                });
            }
            sink(&chunk).map_err(|error| HttpError::Body {
                origin: origin.to_string(),
                reason: error.to_string(),
            })?;
        }
        Ok(produced)
    }
}

/// Classify a transport error into a short reason that is safe to log.
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

fn transport_error(origin: String, error: &reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout {
            phase: if error.is_connect() {
                "connect"
            } else {
                "blob"
            },
        }
    } else {
        HttpError::Transport {
            origin,
            reason: classify(error),
        }
    }
}
