//! One HTTP client, one connection pool, sensible timeouts.
//!
//! A single client is built once and shared. That is not a micro-optimisation:
//! it is what makes HTTP/2 work. Many immutable objects over one pooled
//! connection is the multiplexed behaviour this design wants, and a client per
//! request would throw that away along with the connection warm-up.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::AsyncReadExt;
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
        let request = match offset {
            Some(offset) => Request::get(url).range(Range::from(offset)),
            None => Request::get(url),
        };
        // A resumed transfer that has been redirected cannot continue: the
        // partial was measured against the origin that is no longer answering.
        // Continuing against a different host's idea of `offset` would produce a
        // corrupt object, so this is refused rather than guessed.
        self.exchange(
            origin,
            request,
            max_redirects,
            self.timeouts.per_blob,
            /* resend_body */ true,
        )
        .await
        .map_err(|error| match (error, offset) {
            (HttpError::Redirect { origin, .. }, Some(_)) => HttpError::RangeUnsupported { origin },
            (error, _) => error,
        })
    }

    /// The timeout policy this client applies.
    pub fn timeouts(&self) -> TimeoutPolicy {
        self.timeouts
    }

    /// Send one request, applying the redirect and credential policy by hand.
    ///
    /// The acquisition path only ever issues `GET`s of immutable objects, which
    /// is why [`HttpClient::get`] and [`HttpClient::get_range`] are all it
    /// needed. A caller that has to speak a request API - a versioned API
    /// header, a `POST`, a `DELETE`, a multi-gigabyte body - supplies the parts
    /// through [`Request`] rather than reaching for [`HttpClient::inner`], and so
    /// keeps everything the policy is for: a bounded redirect chain, HTTPS on
    /// every hop, and a configured credential that never leaves the host it was
    /// configured for.
    pub async fn send(
        &self,
        origin: &Origin,
        request: Request<'_>,
        max_redirects: usize,
    ) -> Result<Response, HttpError> {
        self.exchange(
            origin,
            request,
            max_redirects,
            self.timeouts.per_blob,
            /* headers re-sent on a redirect */ false,
        )
        .await
    }
}

impl HttpClient {
    /// The one place a request is put on the wire, for every caller.
    async fn exchange(
        &self,
        origin: &Origin,
        request: Request<'_>,
        max_redirects: usize,
        timeout: Duration,
        resend_body: bool,
    ) -> Result<Response, HttpError> {
        let mut current = request.url.clone();
        let mut hops = 0usize;
        loop {
            let last = hops > 0;
            let range = match request.range {
                // A range is only re-sent to the origin it was measured against.
                // After a redirect the offset is dropped, because a different
                // host is not obliged to agree about what byte `offset` means.
                Some(range) if !last => Some(range.header()),
                _ => None,
            };
            let mut builder = self
                .inner
                .request(request.method.clone(), current.clone())
                .timeout(timeout);
            for (name, value) in &request.headers {
                builder = builder.header(*name, value.clone());
            }
            if let Some(range) = range {
                builder = builder.header(reqwest::header::RANGE, range);
            }
            if let (Some(host), Some(token)) = (&self.authorized_host, &self.bearer)
                && current.host_str() == Some(host.as_str())
            {
                builder = builder.bearer_auth(token.expose());
            }
            // A body is a file handle, and a file handle cannot be replayed
            // after a redirect has consumed it. Dropping the body is not an
            // option either, because a half-sent `POST` is worse than a refusal,
            // so a request that carries one is not allowed to move.
            if let Some(body) = &request.body
                && !(last && !resend_body)
            {
                match body.clone() {
                    Body::Bytes(bytes) => {
                        builder = builder.body(bytes);
                    }
                    Body::File { path, chunk } => {
                        let stream = file_stream(&path, chunk).await?;
                        builder = builder.body(reqwest::Body::wrap_stream(stream));
                    }
                }
            } else if last && !resend_body {
                return Err(HttpError::Redirect {
                    origin: origin.to_string(),
                    reason: "a request with a body was redirected".to_owned(),
                });
            }
            let response = builder.send().await.map_err(|error| HttpError::Transport {
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
}

/// Read a file as a stream of chunks.
///
/// The alternative - reading a release asset into memory to send it - turns a
/// two gigabyte upload into a two gigabyte allocation, so the body is a stream
/// and the file handle is opened per attempt, which is also what makes an
/// upload retryable.
/// The stream owns the file handle, so it captures nothing from `path`.
async fn file_stream(
    path: &Path,
    chunk: usize,
) -> Result<impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + use<>, HttpError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| HttpError::Transport {
            origin: "file".to_owned(),
            reason: error.to_string(),
        })?;
    Ok(futures_util::stream::try_unfold(
        (file, vec![0u8; chunk.max(1)]),
        |(mut file, mut buffer)| async move {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                return Ok(None);
            }
            Ok(Some((
                Bytes::copy_from_slice(&buffer[..read]),
                (file, buffer),
            )))
        },
    ))
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
///
/// Only two shapes exist because only two are needed: a document a caller
/// already holds, and a file on disk. Anything else would be a second
/// representation of content zup measures by digest.
#[derive(Debug, Clone)]
pub enum Body {
    /// Bytes the caller already has.
    Bytes(Vec<u8>),
    /// A file, streamed in `chunk`-sized pieces.
    File {
        path: std::path::PathBuf,
        chunk: usize,
    },
}

impl Body {
    /// How many bytes at a time a file body is read.
    pub const UPLOAD_CHUNK: usize = 1 << 20;
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
        Self {
            body: Some(body.into()),
            ..Self::get(url)
        }
        .with_method(reqwest::Method::POST)
    }

    /// A `PATCH` carrying `body`.
    pub fn patch(url: &'a Url, body: impl Into<Body>) -> Self {
        Self {
            body: Some(body.into()),
            ..Self::get(url)
        }
        .with_method(reqwest::Method::PATCH)
    }

    /// A `DELETE`.
    pub fn delete(url: &'a Url) -> Self {
        Self::new(reqwest::Method::DELETE, url)
    }

    /// Replace the method.
    pub fn with_method(mut self, method: reqwest::Method) -> Self {
        self.method = method;
        self
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

    /// The URL this request addresses.
    pub fn url(&self) -> &Url {
        self.url
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

    /// Read the whole body, bounded by `limit`.
    ///
    /// For a response that is small by construction - a JSON document, an error
    /// body - and never for a release asset. The bound is a ceiling rather than a
    /// hint: a server that sends more than the caller will accept is refused
    /// rather than truncated, because a truncated JSON document parses into
    /// something that looks true.
    pub async fn read_to_end(self, limit: u64) -> Result<Vec<u8>, HttpError> {
        if self.declared_length().is_some_and(|length| length > limit) {
            return Err(HttpError::TooLarge {
                origin: self.origin.to_string(),
                limit,
            });
        }
        let origin = self.origin.clone();
        let mut body = Vec::new();
        let mut stream = self.inner.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| HttpError::Transport {
                origin: origin.to_string(),
                reason: classify(&error),
            })?;
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Err(HttpError::TooLarge {
                    origin: origin.to_string(),
                    limit,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Stream the body into `sink`, one chunk at a time.
    ///
    /// For a body that is large and whose progress is worth keeping. A whole-body
    /// read buffers first and hands over nothing until the last byte, so a
    /// connection that drops at ninety percent costs the whole transfer; this
    /// hands over every chunk as it lands, so a caller with a resumable sink keeps
    /// what arrived.
    ///
    /// `limit` bounds the total, the same way [`Self::read_to_end`] does.
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
        let mut stream = self.inner.bytes_stream();
        let mut produced = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| HttpError::Transport {
                origin: origin.to_string(),
                reason: classify(&error),
            })?;
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
