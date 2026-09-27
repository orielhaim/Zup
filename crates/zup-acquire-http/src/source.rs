//! The HTTP source: fill the verified cache from an ordered set of origins.
//!
//! One blob's transfer, start to finish:
//!
//! 1. Ask the cache for a writer. It resumes a valid partial, or starts clean.
//! 2. If there is a partial, ask for the remaining range and require a correct
//!    `206` with a `Content-Range` that lines up. A server that refuses the
//!    range, or answers with a mismatched one, makes the partial be discarded
//!    and the transfer restart — which is what a CDN without range support
//!    requires, and it costs a full download rather than a wrong file.
//! 3. Stream the body into the writer, bounded by the descriptor's wire length.
//! 4. Commit. The cache decompresses, hashes, and publishes only if the digest
//!    matches.
//!
//! No step trusts the server. Not the status, not `Content-Length`, not
//! `Content-Range`, and not the bytes themselves.

use futures_util::StreamExt;
use zup_acquire::{
    AcquireRequest, ArtifactSource, CacheProbe, Cancellation, ContentDescriptor,
    RelativeContentPath, SourceError, SourceFuture, VerifiedBlob, Verify, cancellable_sleep,
};

use crate::client::{HttpClient, HttpClientConfig, Response};
use crate::error::{HttpError, RetryDecision};
use crate::origin::{Origin, OriginSet};
use crate::retry::{BackoffPolicy, RetryState};

/// A source that fetches blobs over HTTP from an ordered set of origins.
pub struct HttpSource {
    name: String,
    client: HttpClient,
    origins: OriginSet,
    policy: BackoffPolicy,
    max_redirects: usize,
}

impl HttpSource {
    /// A source over `origins`, sharing one client.
    pub fn new(name: impl Into<String>, client: HttpClient, origins: OriginSet) -> Self {
        Self {
            name: name.into(),
            client,
            origins,
            policy: BackoffPolicy::default(),
            max_redirects: 5,
        }
    }

    /// A source with its own client, for a caller that has no other use for one.
    pub fn with_config(
        name: impl Into<String>,
        config: &HttpClientConfig,
        origins: OriginSet,
    ) -> Result<Self, HttpError> {
        Ok(Self::new(name, HttpClient::new(config)?, origins))
    }

    /// The retry policy.
    pub fn with_policy(mut self, policy: BackoffPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// How many redirects one request may follow.
    pub fn with_max_redirects(mut self, hops: usize) -> Self {
        self.max_redirects = hops;
        self
    }

    /// The origins, in the order they will be tried.
    pub fn origins(&self) -> OriginSet {
        self.origins.clone()
    }

    /// Fetch one document, bounded by `limit`.
    ///
    /// A document is a release descriptor, a catalog, or a variant manifest. It
    /// is small, it is authenticated by a digest the caller already has, and it
    /// must be in memory before it can be parsed — so it is read whole and
    /// checked against the caller's descriptor afterwards.
    pub async fn fetch_document(
        &self,
        path: &RelativeContentPath,
        limit: u64,
    ) -> Result<Vec<u8>, HttpError> {
        let mut last: Option<HttpError> = None;
        for origin in self.origins.ordered() {
            let url = self.client.url(origin, path)?;
            let response = self.client.get(origin, &url, self.max_redirects).await?;
            if !response.status().is_success() {
                last = Some(HttpError::Status {
                    origin: origin.to_string(),
                    status: response.status().as_u16(),
                });
                continue;
            }
            if response
                .declared_length()
                .is_some_and(|length| length > limit)
            {
                // A declared length beyond the limit is a refusal, not a
                // truncated read: the server has told us it intends to send
                // more than the document may be.
                return Err(HttpError::TooLarge {
                    origin: origin.to_string(),
                    limit,
                });
            }
            let mut body = Vec::new();
            let mut stream = response.into_body().bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| HttpError::Transport {
                    origin: origin.to_string(),
                    reason: crate::client::classify(&error),
                })?;
                if body.len() as u64 + chunk.len() as u64 > limit {
                    return Err(HttpError::TooLarge {
                        origin: origin.to_string(),
                        limit,
                    });
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(body);
        }
        Err(last.unwrap_or(HttpError::Transport {
            origin: "no origin".to_owned(),
            reason: "no origin is configured".to_owned(),
        }))
    }

    /// Fetch one blob, resuming and retrying across every origin.
    async fn fetch_blob(
        &self,
        descriptor: &ContentDescriptor,
        cache: &zup_acquire::ContentCache,
        cancellation: &dyn Cancellation,
    ) -> Result<VerifiedBlob, HttpError> {
        let path = zup_acquire::WebLayout::blob(&descriptor.digest);
        let mut last: Option<HttpError> = None;
        let mut attempts = 0u32;
        let mut resumed_bytes = 0u64;

        for (index, origin) in self.origins.indices() {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let url = match self.client.url(origin, &path) {
                Ok(url) => url,
                Err(error) => {
                    last = Some(error);
                    continue;
                }
            };
            let mut state = RetryState::new(self.policy, 0);
            loop {
                if cancellation.is_cancelled() {
                    return Err(HttpError::Cancelled);
                }
                attempts += 1;
                let writer = match cache.writer(descriptor) {
                    Ok(writer) => writer,
                    Err(error) => {
                        // The blob is already verified: another process, or an
                        // earlier item in this session, got there first. A
                        // reservation is therefore not a failure, it is a
                        // reason to read rather than to write.
                        return cache
                            .get(descriptor, Verify::for_kind(descriptor.kind()))
                            .ok()
                            .flatten()
                            .ok_or(HttpError::Cache(error));
                    }
                };
                if state.attempt() == 0 {
                    resumed_bytes = writer.wire_offset();
                    state = RetryState::new(self.policy, resumed_bytes);
                }
                let offset = writer.wire_offset();
                let mut attempt = BlobRequest {
                    origin,
                    url: &url,
                    descriptor,
                    cache,
                    writer: Some(writer),
                    offset,
                    cancellation,
                };
                match self.transfer(&mut attempt).await {
                    Ok(blob) => {
                        self.origins.prefer(index);
                        let _ = (attempts, resumed_bytes);
                        return Ok(blob);
                    }
                    Err((error, retry_after)) => {
                        let decision = state.record(&error, retry_after.as_deref());
                        last = Some(error);
                        match decision {
                            RetryDecision::Again(delay) => {
                                // A wait that ignores cancellation turns a
                                // prompt cancel into an apparent hang.
                                if !cancellable_sleep(delay, cancellation).await {
                                    return Err(HttpError::Cancelled);
                                }
                            }
                            RetryDecision::Elsewhere | RetryDecision::Stop => break,
                        }
                    }
                }
            }
        }
        Err(last.unwrap_or(HttpError::Transport {
            origin: "no origin".to_owned(),
            reason: "no origin is configured".to_owned(),
        }))
    }

    /// One request's body, from `offset` to the end of the wire form.
    async fn transfer(
        &self,
        request: &mut BlobRequest<'_>,
    ) -> Result<VerifiedBlob, (HttpError, Option<String>)> {
        let (origin, url, offset) = (request.origin, request.url, request.offset);
        let response = if offset > 0 {
            self.client
                .get_range(origin, url, offset, self.max_redirects)
                .await
        } else {
            self.client.get(origin, url, self.max_redirects).await
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                request.abandon();
                return Err((error, None));
            }
        };
        let descriptor = request.descriptor;

        if !response.status().is_success() {
            let retry_after = response.header("retry-after").map(str::to_owned);
            let error = HttpError::Status {
                origin: origin.to_string(),
                status: response.status().as_u16(),
            };
            request.abandon();
            return Err((error, retry_after));
        }

        if offset > 0 {
            match check_range(origin, &response, offset, descriptor) {
                Ok(()) => {}
                Err(RangeOutcome::Restart) => {
                    // The server will not honour a range. The partial has to go,
                    // not merely be abandoned: abandoning keeps it, and the next
                    // attempt would ask for the same range and be refused the
                    // same way. Deleting it is what makes the documented
                    // fallback — a full download — actually happen.
                    request.discard();
                    return Err((
                        HttpError::RangeUnsupported {
                            origin: origin.to_string(),
                        },
                        None,
                    ));
                }
                Err(RangeOutcome::Refuse(declared)) => {
                    request.abandon();
                    return Err((
                        HttpError::RangeMismatch {
                            origin: origin.to_string(),
                            requested: format!("bytes={offset}-"),
                            declared,
                        },
                        None,
                    ));
                }
            }
        } else if let Some(declared) = response.declared_length()
            && declared != descriptor.compressed_size
        {
            // The server has told us it intends to send a different length than
            // the authenticated descriptor says. Appending to a partial built
            // for a different object is exactly the mistake to refuse.
            request.abandon();
            return Err((
                HttpError::Truncated {
                    origin: origin.to_string(),
                    received: declared,
                    expected: descriptor.compressed_size,
                },
                None,
            ));
        }

        match stream_body(response, origin, request).await {
            Ok(()) => request.commit(),
            Err(error) => {
                // A transfer that stopped early leaves a partial with a verified
                // prefix. The cache decides whether that partial is resumable;
                // this loop only decides whether to try again.
                request.abandon();
                Err((error, None))
            }
        }
    }
}

/// One blob's request, in the state a single attempt needs.
///
/// Grouping these is what keeps `transfer` about the transfer: the state is the
/// same on every attempt, and only the response differs. The writer is an
/// `Option` because finishing a transfer consumes it, and every path out of
/// `transfer` finishes it one way or another.
struct BlobRequest<'a> {
    origin: &'a Origin,
    url: &'a url::Url,
    descriptor: &'a ContentDescriptor,
    cache: &'a zup_acquire::ContentCache,
    writer: Option<zup_acquire::BlobWriter>,
    /// Where the wire form already got to, which is what a range asks for.
    offset: u64,
    cancellation: &'a dyn Cancellation,
}

impl BlobRequest<'_> {
    /// Write wire bytes, if the transfer is still in flight.
    fn write(&mut self, bytes: &[u8]) -> Result<(), HttpError> {
        match &mut self.writer {
            Some(writer) => writer.write(bytes).map_err(HttpError::Cache),
            None => Ok(()),
        }
    }

    /// Finish the transfer, publishing it if every byte arrived.
    fn commit(&mut self) -> Result<VerifiedBlob, (HttpError, Option<String>)> {
        let Some(writer) = self.writer.take() else {
            return Err((
                HttpError::Transport {
                    origin: self.origin.to_string(),
                    reason: "the transfer was already settled".to_owned(),
                },
                None,
            ));
        };
        writer
            .commit()
            .map_err(|error| (HttpError::Cache(error), None))
    }

    /// Leave a partial behind for the next attempt, if one is worth having.
    fn abandon(&mut self) {
        if let Some(writer) = self.writer.take() {
            writer.abandon();
        }
    }

    /// Delete the partial, so the next attempt starts from zero.
    ///
    /// This is the only path that destroys progress. Every other failure keeps
    /// it, because every other failure is a statement about the connection
    /// rather than about the object.
    fn discard(&mut self) {
        if let Ok(paths) = self.cache.paths(self.descriptor) {
            self.cache.discard_partial(&paths);
        }
        self.abandon();
    }
}

enum RangeOutcome {
    Restart,
    Refuse(String),
}

/// Require a correct `206` and a `Content-Range` that lines up with the request.
fn check_range(
    _origin: &Origin,
    response: &Response,
    offset: u64,
    descriptor: &ContentDescriptor,
) -> Result<(), RangeOutcome> {
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(RangeOutcome::Restart);
    }
    let declared = match response.header("content-range") {
        Some(value) => value,
        // A 206 with no range header describes nothing. Restarting is safe; a
        // guess is not.
        None => return Err(RangeOutcome::Restart),
    };
    let total = descriptor.compressed_size;
    // `bytes <start>-<end>/<total>` or `bytes <start>-<end>/*`.
    let body = declared.strip_prefix("bytes ").unwrap_or(declared);
    let (range, size) = match body.split_once('/') {
        Some((range, size)) => (range, size),
        None => return Err(RangeOutcome::Refuse(declared.to_owned())),
    };
    let (start, end) = match range.split_once('-') {
        Some(pair) => pair,
        None => return Err(RangeOutcome::Refuse(declared.to_owned())),
    };
    let start: u64 = match start.trim().parse() {
        Ok(value) => value,
        Err(_) => return Err(RangeOutcome::Refuse(declared.to_owned())),
    };
    if start != offset {
        return Err(RangeOutcome::Refuse(declared.to_owned()));
    }
    if let Ok(declared_total) = size.trim().parse::<u64>()
        && declared_total != total
    {
        return Err(RangeOutcome::Refuse(declared.to_owned()));
    }
    if let Ok(end) = end.trim().parse::<u64>()
        && end + 1 != total
    {
        return Err(RangeOutcome::Refuse(declared.to_owned()));
    }
    Ok(())
}

async fn stream_body(
    response: Response,
    origin: &Origin,
    request: &mut BlobRequest<'_>,
) -> Result<(), HttpError> {
    let mut stream = response.into_inner().bytes_stream();
    while let Some(chunk) = stream.next().await {
        if request.cancellation.is_cancelled() {
            return Err(HttpError::Cancelled);
        }
        let chunk = chunk.map_err(|error| HttpError::Transport {
            origin: origin.to_string(),
            reason: crate::client::classify(&error),
        })?;
        request.write(&chunk)?;
    }
    Ok(())
}

impl ArtifactSource for HttpSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, _descriptor: &ContentDescriptor) -> bool {
        // An HTTP source is always willing. "Does this origin have it" is
        // answered by a request, and a HEAD per blob would double the round
        // trips to learn nothing the digest does not already say.
        !self.origins.is_empty()
    }

    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let descriptor = *request.descriptor;
            // A verified blob is never re-fetched, whatever the origin list says.
            match request
                .cache
                .probe(&descriptor, Verify::for_kind(descriptor.kind()))
            {
                Ok(CacheProbe::Present { .. }) => {
                    return match request
                        .cache
                        .get(&descriptor, Verify::for_kind(descriptor.kind()))
                    {
                        Ok(Some(blob)) => Ok(blob),
                        Ok(None) => Err(SourceError::absent(self.name(), &descriptor)),
                        Err(error) => Err(SourceError::unavailable(
                            self.name(),
                            &descriptor,
                            error.to_string(),
                        )),
                    };
                }
                Ok(CacheProbe::Absent | CacheProbe::Resumable { .. }) => {}
                Err(error) => {
                    return Err(SourceError::unavailable(
                        self.name(),
                        &descriptor,
                        error.to_string(),
                    ));
                }
            }
            self.fetch_blob(&descriptor, request.cache, request.cancellation)
                .await
                .map_err(|error| {
                    if matches!(error, HttpError::Cancelled) {
                        return SourceError::unavailable(self.name(), &descriptor, "cancelled");
                    }
                    SourceError::unavailable(self.name(), &descriptor, error.to_string())
                })
        })
    }
}

/// A document to fetch before any content.
pub struct MetadataRequest {
    pub path: RelativeContentPath,
    pub limit: u64,
}

impl MetadataRequest {
    /// Ask for a document of at most `limit` bytes.
    pub fn new(path: RelativeContentPath, limit: u64) -> Self {
        Self { path, limit }
    }
}

/// Fetch one authenticated document.
///
/// A document is read whole and returned. The caller compares it against a
/// digest from authenticated metadata, which is what makes it trusted; nothing
/// here parses it, and nothing here believes the server's `Content-Length`.
pub async fn fetch_metadata(
    source: &HttpSource,
    request: &MetadataRequest,
) -> Result<Vec<u8>, HttpError> {
    source.fetch_document(&request.path, request.limit).await
}
