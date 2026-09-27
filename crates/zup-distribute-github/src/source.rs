//! Reading a variant's content out of a GitHub release.
//!
//! # What this source is
//!
//! An `ArtifactSource`: given a content descriptor the authenticated release
//! already named, produce a verified blob in the cache. Everything it does is
//! in service of that, and it does nothing else: it does not select a variant,
//! it does not authenticate a release, and it does not decide what to install.
//!
//! ```text
//! authenticated catalog says sha256:A is 4096 / 1200 bytes
//!     ↓
//! the package index says A is at byte 8192, 1200 compressed bytes
//!     ↓
//! one range request  →  206 + Content-Range      →  1200 bytes
//!     ↓ or
//! the whole package   →  200                    →  everything, then find A
//!     ↓
//! decompress, hash, compare with A
//!     ↓
//! publish through the cache, which re-verifies
//! ```
//!
//! # Why range is an optimisation and never a requirement
//!
//! GitHub's Release Asset API does not document HTTP Range support as a provider
//! contract. It may work; it may work today and not next year; and a client that
//! depends on it produces a broken install for some users on the day it stops.
//!
//! So range is probed, not assumed. A correct `206` with a `Content-Range` that
//! lines up means the byte range was really served and the transfer is small. A
//! `200` means the host sent the whole object, and the source reads the frame out
//! of it — correct, slower, and never a failure. A `206` with a `Content-Range`
//! that does *not* line up is a refusal, not a fallback: the host disagrees
//! about what byte `n` is, and reading the wrong bytes is worse than reading all
//! of them.
//!
//! Which happened is recorded, so `zup doctor` can say whether a project's
//! GitHub distribution is actually range-accelerated or quietly downloading
//! whole packages. A source that silently fell back would make the optimisation
//! look like it works.
//!
//! All three outcomes settle the question for the rest of the source's life,
//! including the disagreement: once a host has been shown to misalign a range,
//! probing it again per blob costs a request and learns nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zup_acquire::{
    AcquireRequest, ArtifactSource, BlobWriter, CacheProbe, Cancellation, ContentCatalog,
    ContentDescriptor, SourceError, SourceFuture, VerifiedBlob, Verify,
};
use zup_acquire_http::{HttpClient, HttpClientConfig, Origin, Range, Request};
use zup_core::Sha256Digest;

use crate::descriptor::{PackageDescriptor, ShardRef};
use crate::error::{DistributeError, PackageError};
use crate::layout::ReleaseLayout;
use crate::metrics::Metrics;
use crate::package::{self, Index};

/// How many redirects one asset fetch may follow.
const MAX_REDIRECTS: usize = 5;

/// Whether the host actually served a byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSupport {
    /// Not asked yet.
    Unknown,
    /// A `206` with a matching `Content-Range` came back.
    Supported,
    /// A `200` came back, so the host does not do ranges.
    Unsupported,
}

/// One variant's package, opened.
#[derive(Debug, Clone)]
pub struct Package {
    pub descriptor: PackageDescriptor,
    pub index: Index,
    /// The address of each piece, in index order.
    pub urls: Vec<String>,
}

impl Package {
    /// The piece that holds byte `offset` of the logical package.
    fn piece_at(&self, offset: u64) -> Option<(usize, &ShardRef)> {
        self.descriptor
            .shards
            .iter()
            .enumerate()
            .find(|(_, shard)| {
                offset >= shard.start && offset < shard.start.saturating_add(shard.size)
            })
    }

    /// Whether the package is small enough that a whole fetch is the right move.
    pub fn total(&self) -> u64 {
        self.descriptor.package.size
    }
}

/// A content source that reads one variant's package from a GitHub release.
pub struct GithubContentSource {
    name: String,
    client: HttpClient,
    site: Origin,
    package: Package,
    catalog: ContentCatalog,
    range: Arc<Mutex<RangeSupport>>,
    metrics: Arc<Metrics>,
    max_redirects: usize,
}

impl GithubContentSource {
    /// Build a source over an already-opened package.
    pub fn new(
        name: impl Into<String>,
        layout: &ReleaseLayout,
        package: Package,
        catalog: ContentCatalog,
    ) -> Result<Self, DistributeError> {
        let site = Origin::site(&layout.asset_url("zup-release.json")?)
            .map_err(DistributeError::Repository)?;
        Ok(Self {
            name: name.into(),
            client: HttpClient::new(&HttpClientConfig::default()).map_err(|error| {
                DistributeError::Fetch {
                    name: "client".to_owned(),
                    reason: error.to_string(),
                }
            })?,
            site,
            package,
            catalog,
            range: Arc::new(Mutex::new(RangeSupport::Unknown)),
            metrics: Arc::new(Metrics::default()),
            max_redirects: MAX_REDIRECTS,
        })
    }

    /// Replace the client, for a caller that has one.
    pub fn with_client(mut self, client: HttpClient) -> Self {
        self.client = client;
        self
    }

    /// The metrics this source has collected.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Whether the host has been observed to serve byte ranges.
    pub fn range_support(&self) -> RangeSupport {
        *self
            .range
            .lock()
            .expect("the range observation is not poisoned")
    }

    /// The variant this source serves.
    pub fn variant(&self) -> &str {
        &self.package.descriptor.variant
    }

    /// The digests this source can produce, and where they live in the package.
    pub fn contents(&self) -> BTreeMap<Sha256Digest, u64> {
        self.package
            .index
            .metadata
            .blobs
            .iter()
            .map(|frame| (frame.digest, frame.size))
            .collect()
    }

    /// Whether this source could serve `descriptor` at all.
    ///
    /// Structural, and cheap: the index is already in memory, so this is a
    /// binary search rather than a request. A source that says no here saves the
    /// scheduler from opening a connection it would learn nothing from.
    pub fn carries(&self, descriptor: &ContentDescriptor) -> bool {
        match self.package.index.metadata.frame(&descriptor.digest) {
            Some(frame) => frame.size == descriptor.size,
            None => false,
        }
    }

    /// Fetch one blob's compressed frame straight into the cache, resuming.
    ///
    /// Two strategies, and the choice is recorded either way. A frame goes into
    /// the cache as the wire form it already is: the cache bounds a transfer by
    /// the catalog's declared wire length, and on commit decodes the whole frame
    /// and checks the digest of what came out — so publishing through it is what
    /// verifies, not a step this source repeats in order to be sure. Decoding
    /// here and handing over the result would cost a second full pass over every
    /// blob in the project and assert nothing extra.
    ///
    /// # Why the body is streamed and not read
    ///
    /// Chunks go into the cache as they arrive. Reading a response whole first
    /// would mean a connection that drops at ninety percent of a ninety-megabyte
    /// blob costs all ninety megabytes, because nothing reached the disk — and a
    /// resumable writer that is fed only at the end of a transfer cannot resume.
    /// So the resume offset is the writer's, and each attempt asks for exactly the
    /// bytes the cache does not have.
    async fn publish_frame(
        &self,
        frame: &package::Frame,
        descriptor: &ContentDescriptor,
        writer: &mut BlobWriter,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SourceError> {
        let entry = *frame;
        let (piece, shard) = self.package.piece_at(entry.offset).ok_or_else(|| {
            SourceError::unavailable(
                &self.name,
                descriptor,
                "the package index places this blob outside every piece",
            )
        })?;
        let url = self.package.urls.get(piece).cloned().unwrap_or_default();
        let within = entry.offset - shard.start;
        let end = within + entry.compressed_size - 1;

        while writer.wire_offset() < entry.compressed_size {
            if cancellation.is_cancelled() {
                return Err(cancelled(&self.name, descriptor));
            }
            let already = writer.wire_offset();
            let from = within + already;

            // The probe. One ranged request, and what comes back decides the mode
            // for the rest of this source's life.
            let transfer = if self.range_support() == RangeSupport::Unsupported {
                self.get_whole(&url, descriptor).await?
            } else {
                match self.get_range(&url, from, end, descriptor).await? {
                    Ranged::Exact(response) => {
                        self.observe_range(RangeSupport::Supported);
                        let (wire, written) = self
                            .write_through(
                                response,
                                writer,
                                descriptor,
                                "a frame is the whole response",
                                cancellation,
                            )
                            .await?;
                        self.metrics.ranged(piece);
                        self.metrics.needed(written);
                        self.metrics.request(wire);
                        continue;
                    }
                    Ranged::Whole(response) => {
                        // A 200. The host sent everything, and the frame is inside
                        // it.
                        self.observe_range(RangeSupport::Unsupported);
                        response
                    }
                    Ranged::Mismatch { declared, response } => {
                        // A 206 whose range does not line up. The host and this
                        // client disagree about what byte `from` is, so the bytes
                        // cannot be trusted at all — the whole object is the only
                        // correct answer.
                        //
                        // And the host is not asked again for the rest of this
                        // source's life: a misaligned `Content-Range` is a property
                        // of the host, not of one unlucky frame, so re-probing
                        // every blob would buy a second wasted request per blob in
                        // exchange for finding out nothing new.
                        self.metrics.refused(declared);
                        self.observe_range(RangeSupport::Unsupported);
                        // The probe's body crossed the wire and is being thrown
                        // away, so it is counted: a report that hid it would make a
                        // misbehaving host look cheaper than it is.
                        let discarded = drain(response, descriptor, &self.name).await?;
                        self.metrics.request(discarded);
                        self.get_whole(&url, descriptor).await?
                    }
                }
            };
            self.metrics.fallback();
            // The frame's bytes are somewhere inside a whole piece. Everything
            // before them is discarded as it arrives rather than buffered, because
            // a piece can be larger than memory and there is no reason to hold the
            // part of it this frame is not made of.
            //
            // The two numbers are different and both are worth having: `wire` is
            // what the host sent, which is the cost, and `written` is what the
            // release needed, which is what the saving would have been.
            let (wire, written) = self
                .write_window(
                    transfer,
                    writer,
                    descriptor,
                    from,
                    within + entry.compressed_size,
                    cancellation,
                )
                .await?;
            self.metrics.request(wire);
            self.metrics.needed(written);
        }
        Ok(())
    }

    /// Write a response that *is* the frame, chunk by chunk.
    async fn write_through(
        &self,
        response: zup_acquire_http::Response,
        writer: &mut BlobWriter,
        descriptor: &ContentDescriptor,
        what: &'static str,
        cancellation: &dyn Cancellation,
    ) -> Result<(u64, u64), SourceError> {
        let name = self.name.clone();
        let wire = response
            .stream_into(u64::MAX, |chunk| {
                if cancellation.is_cancelled() {
                    return Err(std::io::Error::other("cancelled"));
                }
                writer.write(chunk).map_err(io)
            })
            .await
            .map_err(|error| refuse(&name, descriptor, what, error))?;
        Ok((wire, wire))
    }

    /// Write the bytes of a whole piece that fall inside `[from, to)`.
    ///
    /// Returns `(bytes received, bytes written)`.
    async fn write_window(
        &self,
        response: zup_acquire_http::Response,
        writer: &mut BlobWriter,
        descriptor: &ContentDescriptor,
        from: u64,
        to: u64,
        cancellation: &dyn Cancellation,
    ) -> Result<(u64, u64), SourceError> {
        if from >= to {
            return Err(SourceError::unavailable(
                &self.name,
                descriptor,
                "the package index places this blob outside its own piece",
            ));
        }
        let name = self.name.clone();
        // Byte offsets within the current chunk, so a chunk that straddles the
        // window's edges contributes only the part that is inside it.
        let mut position = 0u64;
        let mut written = 0u64;
        let wire = response
            .stream_into(u64::MAX, |chunk| {
                if cancellation.is_cancelled() {
                    return Err(std::io::Error::other("cancelled"));
                }
                let start = position;
                let end = start + chunk.len() as u64;
                position = end;
                if end <= from || start >= to {
                    return Ok(());
                }
                let low = from.saturating_sub(start) as usize;
                let high = (to.min(end) - start) as usize;
                writer.write(&chunk[low..high]).map_err(io)?;
                written += (high - low) as u64;
                Ok(())
            })
            .await
            .map_err(|error| {
                refuse(
                    &name,
                    descriptor,
                    "a piece that stops inside the frame it was asked for",
                    error,
                )
            })?;
        if position < to {
            return Err(SourceError::unavailable(
                &self.name,
                descriptor,
                format!("the piece is {position} bytes and the frame ends at {to}"),
            ));
        }
        Ok((wire, written))
    }

    /// Fetch a whole piece.
    async fn get_whole(
        &self,
        url: &str,
        descriptor: &ContentDescriptor,
    ) -> Result<zup_acquire_http::Response, SourceError> {
        let address = url::Url::parse(url)
            .map_err(|error| SourceError::unavailable(&self.name, descriptor, error.to_string()))?;
        let response = self
            .client
            .get(&self.site, &address, self.max_redirects)
            .await
            .map_err(|error| SourceError::unavailable(&self.name, descriptor, error.reason()))?;
        if !response.status().is_success() {
            return Err(SourceError::unavailable(
                &self.name,
                descriptor,
                format!("status {}", response.status().as_u16()),
            ));
        }
        Ok(response)
    }

    /// Ask for one byte range, and classify what came back.
    async fn get_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
        descriptor: &ContentDescriptor,
    ) -> Result<Ranged, SourceError> {
        let address = url::Url::parse(url)
            .map_err(|error| SourceError::unavailable(&self.name, descriptor, error.to_string()))?;
        let response = self
            .client
            .send(
                &self.site,
                Request::get(&address).range(Range::between(start, end)),
                self.max_redirects,
            )
            .await
            .map_err(|error| SourceError::unavailable(&self.name, descriptor, error.reason()))?;
        if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let declared = response.header("content-range").map(str::to_owned);
            let agrees = declared
                .as_deref()
                .is_some_and(|declared| content_range_agrees(declared, start, end));
            if !agrees {
                // Either a 206 that describes nothing, or one that describes a
                // range other than the one asked for. Both are the same event: a
                // claim this client cannot check, and bytes it must not use.
                return Ok(Ranged::Mismatch {
                    declared: declared.unwrap_or_else(|| "no content-range".to_owned()),
                    response,
                });
            }
            return Ok(Ranged::Exact(response));
        }
        if response.status().is_success() {
            return Ok(Ranged::Whole(response));
        }
        Err(SourceError::unavailable(
            &self.name,
            descriptor,
            format!("status {}", response.status().as_u16()),
        ))
    }

    fn observe_range(&self, support: RangeSupport) {
        let mut slot = self
            .range
            .lock()
            .expect("the range observation is not poisoned");
        // One observation wins for the source's life. A host that answered a
        // range correctly once is assumed to keep doing so, and a host that
        // answered with the whole object is never asked again, because the
        // fallback is free of surprises and the probe is not.
        if *slot == RangeSupport::Unknown {
            *slot = support;
        }
    }
}

enum Ranged {
    /// A `206` whose range is the one asked for, and the body is that range.
    Exact(zup_acquire_http::Response),
    /// A `200`: the body is the whole piece, from byte zero.
    Whole(zup_acquire_http::Response),
    /// The host declared a range other than the one asked for. The body is
    /// discarded, and its length counted, because it crossed the wire.
    Mismatch {
        declared: String,
        response: zup_acquire_http::Response,
    },
}

fn cancelled(source: &str, descriptor: &ContentDescriptor) -> SourceError {
    SourceError::unavailable(source, descriptor, "cancelled")
}

/// A cache refusal as the `io::Error` a body sink reports.
///
/// The `CacheError` is not in the message: a sink that refused is either a bound
/// or a mismatch, and the caller reports the surrounding fact — which blob, which
/// piece — which is more use than the reason the writer has already decided to
/// keep private.
fn io(error: zup_acquire::CacheError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// Turn a transport or sink failure into a source error that says what was being
/// read, so a short body is not reported as a bare "connection failed".
fn refuse(
    source: &str,
    descriptor: &ContentDescriptor,
    what: &'static str,
    error: zup_acquire_http::HttpError,
) -> SourceError {
    SourceError::unavailable(source, descriptor, format!("{what}: {}", error.reason()))
}

/// Read a response to nothing, for the bytes that are being discarded.
async fn drain(
    response: zup_acquire_http::Response,
    descriptor: &ContentDescriptor,
    source: &str,
) -> Result<u64, SourceError> {
    response
        .stream_into(u64::MAX, |_| Ok(()))
        .await
        .map_err(|error| {
            SourceError::unavailable(
                source,
                descriptor,
                format!("a discarded body failed: {}", error.reason()),
            )
        })
}

/// Whether a `Content-Range` describes the bytes that were asked for.
fn content_range_agrees(declared: &str, start: u64, end: u64) -> bool {
    let body = declared.strip_prefix("bytes ").unwrap_or(declared);
    let Some((range, _total)) = body.split_once('/') else {
        return false;
    };
    let Some((from, to)) = range.split_once('-') else {
        return false;
    };
    matches!((from.trim().parse::<u64>(), to.trim().parse::<u64>()), (Ok(a), Ok(b)) if a == start && b == end)
}

impl ArtifactSource for GithubContentSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.carries(descriptor)
    }

    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let descriptor = *request.descriptor;
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
                        Ok(None) => Err(SourceError::absent(&self.name, &descriptor)),
                        Err(error) => Err(SourceError::unavailable(
                            &self.name,
                            &descriptor,
                            error.to_string(),
                        )),
                    };
                }
                Ok(CacheProbe::Absent | CacheProbe::Resumable { .. }) => {}
                Err(error) => {
                    return Err(SourceError::unavailable(
                        &self.name,
                        &descriptor,
                        error.to_string(),
                    ));
                }
            }
            let Some(frame) = self
                .package
                .index
                .metadata
                .frame(&descriptor.digest)
                .copied()
            else {
                return Err(SourceError::absent(&self.name, &descriptor));
            };
            if frame.size != descriptor.size {
                return Err(SourceError::unavailable(
                    &self.name,
                    &descriptor,
                    format!(
                        "the package says sha256:{} is {} bytes; the release says {}",
                        descriptor.digest.to_hex(),
                        frame.size,
                        descriptor.size
                    ),
                ));
            }
            if self.catalog.entry(&descriptor.digest).is_none() {
                // The package may name a blob the authenticated catalog does not.
                // Importing it would put content in the cache that no release
                // ever claimed, so it is refused rather than tolerated.
                return Err(SourceError::unavailable(
                    &self.name,
                    &descriptor,
                    format!(
                        "the authenticated catalog does not name sha256:{}",
                        descriptor.digest.to_hex()
                    ),
                ));
            }
            if request.cancellation.is_cancelled() {
                return Err(SourceError::unavailable(
                    &self.name,
                    &descriptor,
                    "cancelled",
                ));
            }
            let mut writer = request.cache.writer(&descriptor).map_err(|error| {
                SourceError::unavailable(&self.name, &descriptor, error.to_string())
            })?;
            // The writer is opened before the transfer so its resume offset is
            // known: a partial left by an interrupted attempt is continued from,
            // not re-fetched.
            if writer.wire_offset() > 0 {
                self.metrics.resumed();
            }
            self.publish_frame(&frame, &descriptor, &mut writer, request.cancellation)
                .await?;
            if writer.wire_offset() != frame.compressed_size {
                return Err(SourceError::unavailable(
                    &self.name,
                    &descriptor,
                    format!(
                        "the package indexes {} compressed bytes and the host delivered {}",
                        frame.compressed_size,
                        writer.wire_offset()
                    ),
                ));
            }
            writer.commit().map_err(|error| {
                SourceError::unavailable(&self.name, &descriptor, error.to_string())
            })
        })
    }
}

/// Open a package by reading its first piece and its index.
///
/// The first piece is where the header and the metadata are, which is what makes
/// a sharded package openable by fetching one small file.
pub async fn open(
    layout: &ReleaseLayout,
    descriptor: &PackageDescriptor,
    client: &HttpClient,
) -> Result<Package, DistributeError> {
    let site = Origin::site(&layout.asset_url("zup-release.json")?)?;
    let urls: Vec<String> = descriptor
        .shards
        .iter()
        .map(|shard| {
            layout
                .asset_url(&shard.name)
                .map(|url| url.to_string())
                .map_err(DistributeError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let first = urls
        .first()
        .ok_or_else(|| DistributeError::Repository("the descriptor names no pieces".to_owned()))?;
    let address =
        url::Url::parse(first).map_err(|error| DistributeError::Repository(error.to_string()))?;
    let response = client
        .get(&site, &address, MAX_REDIRECTS)
        .await
        .map_err(|error| DistributeError::Fetch {
            name: descriptor.shards[0].name.clone(),
            reason: error.reason(),
        })?;
    if !response.status().is_success() {
        return Err(DistributeError::Fetch {
            name: descriptor.shards[0].name.clone(),
            reason: format!("status {}", response.status().as_u16()),
        });
    }
    let prefix = response
        .into_body()
        .bytes()
        .await
        .map_err(|error| DistributeError::Fetch {
            name: descriptor.shards[0].name.clone(),
            reason: crate::safe_reason(&error),
        })?;
    let index = Index::read(&prefix).map_err(|error| match error {
        PackageError::Short { .. } => DistributeError::Fetch {
            name: descriptor.shards[0].name.clone(),
            reason: "the first piece is too short to hold a package header".to_owned(),
        },
        other => DistributeError::Package(other),
    })?;
    if index.metadata.variant != descriptor.variant {
        return Err(DistributeError::Untrusted {
            name: descriptor.shards[0].name.clone(),
            reason: format!(
                "the package holds variant `{}` and the descriptor names `{}`",
                index.metadata.variant, descriptor.variant
            ),
        });
    }
    if index.metadata.shards.len() != descriptor.shards.len() {
        return Err(DistributeError::Untrusted {
            name: descriptor.shards[0].name.clone(),
            reason: format!(
                "the package is in {} pieces and the descriptor names {}",
                index.metadata.shards.len(),
                descriptor.shards.len()
            ),
        });
    }
    Ok(Package {
        descriptor: descriptor.clone(),
        index,
        urls,
    })
}

/// Read a package descriptor from a release.
pub async fn load_descriptor(
    layout: &ReleaseLayout,
    variant: &str,
    expected: Option<Sha256Digest>,
    client: &HttpClient,
) -> Result<PackageDescriptor, DistributeError> {
    let name = zup_publish::document_name(&zup_publish::DocumentKind::Package { variant });
    let site = Origin::site(&layout.asset_url(&name)?)?;
    let address = layout
        .asset_url(&name)
        .map_err(|error| DistributeError::Repository(error.to_string()))?;
    let response = client
        .get(&site, &address, MAX_REDIRECTS)
        .await
        .map_err(|error| DistributeError::Fetch {
            name: name.clone(),
            reason: error.reason(),
        })?;
    if !response.status().is_success() {
        return Err(DistributeError::Fetch {
            name,
            reason: format!("status {}", response.status().as_u16()),
        });
    }
    let bytes = response
        .into_body()
        .bytes()
        .await
        .map_err(|error| DistributeError::Fetch {
            name: name.clone(),
            reason: crate::safe_reason(&error),
        })?;
    PackageDescriptor::parse(&bytes, expected)
}

/// A client with default settings, for a caller that has none.
pub fn client() -> Result<HttpClient, DistributeError> {
    HttpClient::new(&HttpClientConfig::default())
        .map_err(|error| DistributeError::Repository(error.to_string()))
}
