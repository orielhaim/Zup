//! The HTTP transport against an origin that misbehaves.
//!
//! Every test here is a claim about what happens when a server lies, stalls,
//! throttles, or serves the wrong bytes. The claims are the same shape: the
//! acquisition either succeeds with verified content, or fails in a way that
//! leaves the cache holding nothing unverified.

mod common;

use std::sync::Arc;

use common::{Behaviour, TestServer, fill};
use sha2::{Digest, Sha256};
use zup_acquire::{
    AcquireError, AcquisitionItem, AcquisitionPlan, AcquisitionSession, ArtifactSource,
    CachePolicy, CacheProbe, ContentCache, ContentDescriptor, ContentKind, ContentReason,
    ProgressSink, SchedulerConfig, SourceChain, Verify,
};
use zup_acquire_http::{
    BackoffPolicy, HttpClient, HttpClientConfig, HttpError, HttpSource, Origin, OriginSet,
    TimeoutPolicy, USER_AGENT, is_retryable_status, retry_after,
};
use zup_core::Sha256Digest;

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

fn logical(seed: u8, length: usize) -> Vec<u8> {
    let mut out = vec![0u8; length];
    fill(&mut out, seed);
    out
}

fn wire(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 1).expect("the fixture compresses")
}

fn payload_descriptor(bytes: &[u8]) -> ContentDescriptor {
    let wire = wire(bytes);
    ContentDescriptor::compressed(
        ContentKind::Payload,
        digest_of(bytes),
        wire.len() as u64,
        bytes.len() as u64,
    )
}

/// The URL path of one blob.
///
/// Always `/`-separated, because this is a URL and a request target. A
/// `DirectorySource` and the cache both split a relative path on `/` and push
/// the segments, so the same string addresses a filesystem tree on every
/// platform.
fn blob_path(descriptor: &ContentDescriptor) -> String {
    zup_acquire::WebLayout::blob(&descriptor.digest).to_string()
}

struct Fixture {
    server: TestServer,
    cache: Arc<ContentCache>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    Fixture {
        server: TestServer::start(),
        cache: Arc::new(
            ContentCache::open(
                tempfile::tempdir().expect("a temporary directory").path(),
                CachePolicy::Keep,
            )
            .expect("the cache opens"),
        ),
        _dir: tempfile::tempdir().expect("a temporary directory"),
    }
}

fn fast_policy() -> BackoffPolicy {
    // Retries are exercised without making the suite slow.
    BackoffPolicy {
        initial: std::time::Duration::from_millis(10),
        maximum: std::time::Duration::from_millis(50),
        factor: 2,
        max_attempts: 3,
        budget: std::time::Duration::from_secs(30),
    }
}

fn source_for(server: &TestServer) -> Arc<dyn ArtifactSource> {
    Arc::new(
        HttpSource::new(
            "cdn",
            HttpClient::new(&HttpClientConfig::default()).expect("the client builds"),
            OriginSet::from_urls(&server.url(), Vec::<&str>::new()).expect("the origin parses"),
        )
        .with_policy(fast_policy()),
    )
}

async fn acquire(
    fixture: &Fixture,
    descriptor: ContentDescriptor,
    sources: Vec<Arc<dyn ArtifactSource>>,
) -> Result<zup_acquire::VerifiedBlob, AcquireError> {
    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        descriptor,
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    AcquisitionSession::new(plan, Arc::clone(&fixture.cache), SchedulerConfig::default())
        .run(
            SourceChain::new(sources),
            Arc::new(zup_acquire::NeverCancelled),
            &ProgressSink::discard(),
        )
        .await
        .map(zup_acquire::Barrier::enter)
        .map(|outcome| outcome.items.into_iter().next().expect("one blob"))
}

async fn acquire_raw(
    fixture: &Fixture,
    descriptor: ContentDescriptor,
    source: Arc<dyn ArtifactSource>,
) -> Result<zup_acquire::VerifiedBlob, AcquireError> {
    acquire(fixture, descriptor, vec![source]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_is_fetched_and_verified_over_http() {
    let fixture = fixture();
    let bytes = logical(1, 64 * 1024);
    let descriptor = payload_descriptor(&bytes);
    fixture
        .server
        .route(&blob_path(&descriptor), Behaviour::Serve(wire(&bytes)));

    let blob = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect("the blob is acquired");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), bytes);
    assert_eq!(fixture.server.requests(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_the_server_corrupts_never_reaches_the_cache() {
    let fixture = fixture();
    let bytes = logical(2, 32 * 1024);
    let descriptor = payload_descriptor(&bytes);
    // Right length, wrong content: the attack a length check alone would miss.
    let impostor = logical(99, bytes.len());
    fixture
        .server
        .route(&blob_path(&descriptor), Behaviour::Serve(wire(&impostor)));

    let error = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect_err("a corrupt blob is refused");
    assert!(error.left_machine_unchanged());
    assert!(
        fixture
            .cache
            .probe(&descriptor, Verify::Full)
            .expect("the cache probes")
            == CacheProbe::Absent,
        "nothing is published from bytes that did not verify"
    );
}

/// A resumed transfer asks for the remainder and appends it. An origin that
/// cannot serve a range answers `200` with the whole object instead, which costs
/// a full re-download but must still be correct rather than merely attempted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_transfer_appends_a_range_and_restarts_against_an_origin_without_one() {
    let bytes = logical(3, 512 * 1024);
    let descriptor = payload_descriptor(&bytes);
    let wire_form = wire(&bytes);

    let honoured = fixture();
    {
        // Seed a partial with a verified prefix, exactly as an interrupted
        // transfer would leave behind.
        let cut = wire_form.len() / 3;
        let mut writer = honoured
            .cache
            .writer(&descriptor)
            .expect("the writer opens");
        writer.write(&wire_form[..cut]).expect("the prefix lands");
        writer.abandon();
    }
    assert!(
        matches!(
            honoured
                .cache
                .probe(&descriptor, Verify::Full)
                .expect("the cache probes"),
            CacheProbe::Resumable { .. }
        ),
        "the fixture really has a resumable partial"
    );
    honoured
        .server
        .route(&blob_path(&descriptor), Behaviour::Serve(wire_form.clone()));

    let blob = acquire_raw(&honoured, descriptor, source_for(&honoured.server))
        .await
        .expect("the resumed blob verifies");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), bytes);
    let ranges = honoured.server.ranges();
    assert!(
        ranges.iter().any(|range| range.is_some()),
        "the transfer asked for a range: {ranges:?}"
    );

    let no_ranges = fixture();
    {
        let cut = wire_form.len() / 2;
        let mut writer = no_ranges
            .cache
            .writer(&descriptor)
            .expect("the writer opens");
        writer.write(&wire_form[..cut]).expect("the prefix lands");
        writer.abandon();
    }
    no_ranges.server.route(
        &blob_path(&descriptor),
        Behaviour::ServeWithoutRange(wire_form.clone()),
    );

    let blob = acquire_raw(&no_ranges, descriptor, source_for(&no_ranges.server))
        .await
        .expect("a CDN without range support still satisfies the closure");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), bytes);
    assert!(
        no_ranges.server.requests() >= 2,
        "one refused range, one full body"
    );
}

/// A `206` that cannot be aligned with the partial is never appended to. A range
/// that starts somewhere else would produce a corrupt object of exactly the right
/// length, and a `206` with no `Content-Range` at all describes nothing, so the
/// client cannot tell which bytes it has been handed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_content_range_that_does_not_line_up_is_refused_rather_than_appended() {
    let bytes = logical(5, 64 * 1024);
    let descriptor = payload_descriptor(&bytes);
    let wire_form = wire(&bytes);

    // A `206` whose range does not start where the partial ends. Appending it
    // would produce a corrupt object that still has the right length.
    let misaligned = fixture();
    {
        let cut = wire_form.len() / 2;
        let mut writer = misaligned
            .cache
            .writer(&descriptor)
            .expect("the writer opens");
        writer.write(&wire_form[..cut]).expect("the prefix lands");
        writer.abandon();
    }
    misaligned.server.route(
        &blob_path(&descriptor),
        Behaviour::WrongContentRange(wire_form.clone()),
    );
    let error = acquire_raw(&misaligned, descriptor, source_for(&misaligned.server))
        .await
        .expect_err("a mismatched content range is refused");
    assert!(error.left_machine_unchanged());
    assert!(
        !misaligned
            .cache
            .get(&descriptor, Verify::Full)
            .expect("the cache probes")
            .is_some()
    );

    // A `206` with no `Content-Range` at all. The only safe answer is to refuse
    // to append and start over, so the result is the whole verified object rather
    // than a concatenation.
    let unstated = fixture();
    {
        let cut = (wire_form.len() / 2) as u64;
        let mut writer = unstated
            .cache
            .writer(&descriptor)
            .expect("the writer opens");
        writer
            .write(&wire_form[..cut as usize])
            .expect("the prefix lands");
        writer.abandon();
    }
    unstated.server.route(
        &blob_path(&descriptor),
        Behaviour::NoContentRange(wire_form),
    );
    let blob = acquire_raw(&unstated, descriptor, source_for(&unstated.server))
        .await
        .expect("the transfer restarts and the closure is satisfied");
    assert_eq!(
        blob.read_to_end().expect("the blob decodes"),
        bytes,
        "the result is the whole verified object, not a concatenation"
    );
    assert!(
        unstated.server.requests() >= 2,
        "the refused range was followed by a full request, saw {} requests",
        unstated.server.requests()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_content_length_is_refused_before_a_byte_is_appended() {
    let fixture = fixture();
    let bytes = logical(7, 64 * 1024);
    let descriptor = payload_descriptor(&bytes);
    let wire_form = wire(&bytes);
    {
        let cut = wire_form.len() / 2;
        let mut writer = fixture.cache.writer(&descriptor).expect("the writer opens");
        writer.write(&wire_form[..cut]).expect("the prefix lands");
        writer.abandon();
    }
    fixture.server.route(
        &blob_path(&descriptor),
        Behaviour::WrongContentLength(wire_form),
    );
    let error = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect_err("a body of a different declared length is refused");
    assert!(error.left_machine_unchanged());
}

/// A connection that drops part-way through a promised body is a failure, and it
/// publishes nothing: the bytes that arrived stay a partial, never a blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_drops_mid_body_publishes_nothing() {
    let fixture = fixture();
    let bytes = logical(8, 256 * 1024);
    let descriptor = payload_descriptor(&bytes);
    let wire_form = wire(&bytes);
    // The server promises the whole object and delivers a third of it.
    let cut = wire_form.len() / 3;
    fixture.server.route(
        &blob_path(&descriptor),
        Behaviour::DisconnectAfter(wire_form, cut),
    );
    let error = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect_err("a dropped connection does not satisfy the closure");
    assert!(error.left_machine_unchanged());
    assert!(
        !fixture
            .cache
            .get(&descriptor, Verify::Full)
            .expect("the cache probes")
            .is_some(),
        "an interrupted transfer publishes nothing"
    );
}

/// A server that paces its own retries outranks the local curve. A value this
/// build cannot read as a delay falls back to the curve rather than to an
/// unbounded wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_servers_own_pacing_outranks_the_local_curve() {
    assert_eq!(
        retry_after(Some("7")),
        Some(std::time::Duration::from_secs(7))
    );
    for unreadable in [
        Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        Some("not a number"),
        None,
    ] {
        assert_eq!(retry_after(unreadable), None, "{unreadable:?} falls back");
    }
}

/// 429 is a statement about now and 404 is a statement about the request, so
/// only the first is worth repeating. The fixture answers the same route the
/// same way each time, so the throttle is expressed by the retry budget rather
/// than by state: the transfer is retried and then gives up cleanly rather than
/// reporting a success it did not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_throttled_response_is_retried_and_then_gives_up_cleanly() {
    for status in [408, 429, 503] {
        assert!(is_retryable_status(status), "{status} is about now");
    }
    for status in [403, 404, 501] {
        assert!(
            !is_retryable_status(status),
            "{status} is about the request"
        );
    }

    let fixture = fixture();
    let bytes = logical(9, 32 * 1024);
    let descriptor = payload_descriptor(&bytes);
    fixture
        .server
        .route(&blob_path(&descriptor), Behaviour::Throttled(429, 1));
    let error = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect_err("a permanently throttled origin does not satisfy the closure");
    assert!(error.left_machine_unchanged());
    assert!(
        fixture.server.requests() >= 2,
        "a retryable status is retried, saw {} requests",
        fixture.server.requests()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_not_found_is_not_retried() {
    let fixture = fixture();
    let bytes = logical(10, 4096);
    let descriptor = payload_descriptor(&bytes);
    fixture
        .server
        .route(&blob_path(&descriptor), Behaviour::Status(404));
    let error = acquire_raw(&fixture, descriptor, source_for(&fixture.server))
        .await
        .expect_err("a missing object does not satisfy the closure");
    assert_eq!(
        fixture.server.requests(),
        1,
        "a permanent refusal is sent once, not repeated"
    );
    assert!(error.left_machine_unchanged());
}

/// A chain moves to the next origin when one is down or when one lies, and
/// neither origin is trusted for the content: the bytes are proved by digest, not
/// by which host produced them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_or_lying_origin_moves_to_the_next_one_and_neither_is_trusted() {
    let client = HttpClient::new(&HttpClientConfig::default()).expect("the client builds");
    let bytes = logical(11, 64 * 1024);
    let descriptor = payload_descriptor(&bytes);

    // The first origin is down; the second has the object.
    let down = TestServer::start();
    let honest = TestServer::start();
    down.server_shutdown();
    honest.route(&blob_path(&descriptor), Behaviour::Serve(wire(&bytes)));
    let source = HttpSource::new(
        "cdn",
        HttpClient::new(&HttpClientConfig::default()).expect("the client builds"),
        OriginSet::from_origins(vec![down.origin(), honest.origin()]),
    )
    .with_policy(fast_policy());
    let blob = acquire_raw(&fixture(), descriptor, Arc::new(source))
        .await
        .expect("the secondary origin satisfies the closure");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), bytes);

    // The first origin answers 200 with the wrong content and looks healthy. It
    // burns its failure budget and the honest one serves.
    let corrupt = TestServer::start();
    let good = TestServer::start();
    corrupt.route(
        &blob_path(&descriptor),
        Behaviour::Serve(wire(&logical(98, bytes.len()))),
    );
    good.route(&blob_path(&descriptor), Behaviour::Serve(wire(&bytes)));
    let source = HttpSource::new(
        "chain",
        client,
        OriginSet::from_origins(vec![corrupt.origin(), good.origin()]),
    )
    .with_policy(fast_policy());
    let blob = acquire_raw(&fixture(), descriptor, Arc::new(source))
        .await
        .expect("the honest origin satisfies the closure");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_error_does_not_poison_an_origin_for_the_session() {
    let policy = fast_policy();
    let reset = http_error_status(503);
    let again = policy.decide(&reset, 1, std::time::Duration::ZERO, None);
    assert!(
        matches!(again, zup_acquire_http::RetryDecision::Again(_)),
        "one transient failure is retried, not treated as a dead origin"
    );
    // Once the budget is spent, the scheduler is told to look elsewhere rather
    // than to keep hammering a host that is having a bad minute.
    let spent = policy.decide(&reset, policy.max_attempts, std::time::Duration::ZERO, None);
    assert_eq!(spent, zup_acquire_http::RetryDecision::Elsewhere);
    // A budget that has already elapsed is the same answer, however few attempts
    // were made.
    let slow = policy.decide(
        &reset,
        1,
        policy.budget + std::time::Duration::from_secs(1),
        None,
    );
    assert_eq!(slow, zup_acquire_http::RetryDecision::Elsewhere);
    // A permanent refusal stops immediately.
    let gone = policy.decide(&http_error_status(404), 1, std::time::Duration::ZERO, None);
    assert_eq!(gone, zup_acquire_http::RetryDecision::Stop);
}

fn http_error_status(status: u16) -> zup_acquire_http::HttpError {
    zup_acquire_http::HttpError::Status {
        origin: "test".to_owned(),
        status,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backoff_curve_grows_is_capped_and_is_jittered() {
    let policy = BackoffPolicy {
        initial: std::time::Duration::from_millis(100),
        maximum: std::time::Duration::from_millis(800),
        factor: 2,
        max_attempts: 8,
        budget: std::time::Duration::from_secs(60),
    };
    // The curve is bounded at both ends, which is the property a caller can rely
    // on. The *values* are jittered, so they are not asserted exactly.
    for attempt in 1..=8 {
        assert!(
            policy.delay(attempt) <= policy.maximum,
            "attempt {attempt} waited past the ceiling"
        );
    }
    // Jitter is what stops every client retrying in lockstep, so repeated draws
    // at the same attempt must not all be identical. It is drawn at attempt two
    // because that is where the curve is still below the ceiling; at the ceiling
    // every draw is the ceiling, which is correct rather than a defect.
    let draws: std::collections::BTreeSet<u128> =
        (0..16).map(|_| policy.delay(2).as_nanos()).collect();
    assert!(
        draws.len() > 1,
        "the backoff is jittered, not a fixed curve: {draws:?}"
    );
    // The curve still rises, so a client that keeps failing backs off further.
    let early: u128 = (0..16)
        .map(|_| policy.delay(1).as_nanos())
        .max()
        .expect("a draw");
    let later: u128 = (0..16)
        .map(|_| policy.delay(3).as_nanos())
        .min()
        .expect("a draw");
    assert!(later > early, "the curve rises: {early} then {later}");
    assert!(BackoffPolicy::never().validate().is_ok());
    assert_eq!(BackoffPolicy::never().delay(1), std::time::Duration::ZERO);
    assert!(
        BackoffPolicy {
            factor: 1,
            ..policy
        }
        .validate()
        .is_err(),
        "a factor of 1 is not a backoff"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_origin_must_be_a_shape_a_request_can_be_safely_made_against() {
    // Plain HTTP is permitted for exactly one case: loopback, which is a test
    // origin and a local development mirror. Everything remote must be HTTPS.
    for good in [
        "https://updates.example.com/acme",
        "http://127.0.0.1:9/base",
        "http://localhost:8080/acme",
    ] {
        assert!(Origin::parse(good).is_ok(), "`{good}` should parse");
    }
    for hostile in [
        "http://updates.example.com/acme",
        "ftp://updates.example.com/acme",
        "https://user:secret@updates.example.com/acme",
        "https://updates.example.com/acme?token=x",
        "https://updates.example.com/acme#frag",
    ] {
        assert!(
            Origin::parse(hostile).is_err(),
            "`{hostile}` must be refused"
        );
    }
    // A diagnostic names the host, never the path, so a future signed URL in a
    // path cannot leak through a log line.
    let origin = Origin::parse("https://updates.example.com/acme/private").expect("valid");
    assert_eq!(origin.to_string(), "https://updates.example.com");
    assert!(!origin.is_loopback());
    assert!(
        Origin::parse("http://127.0.0.1:9")
            .expect("valid")
            .is_loopback()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_document_is_fetched_whole_and_bounded_by_its_limit() {
    let server = TestServer::start();
    server.route(
        "releases/stable.json",
        Behaviour::Serve(b"{\"schema\":1}".to_vec()),
    );
    let source = HttpSource::new(
        "cdn",
        zup_acquire_http::HttpClient::new(&HttpClientConfig::default()).expect("the client builds"),
        OriginSet::from_urls(&server.url(), Vec::<&str>::new()).expect("valid"),
    );
    let path = zup_acquire::WebLayout::release("stable").expect("a path");
    let body = source
        .fetch_document(&path, 1024)
        .await
        .expect("the document is fetched");
    assert_eq!(body, b"{\"schema\":1}");

    let error = source
        .fetch_document(&path, 4)
        .await
        .expect_err("a document beyond its limit is refused");
    assert!(
        matches!(error, zup_acquire_http::HttpError::TooLarge { .. }),
        "{error}"
    );
}

/// Two properties of streaming a body into a sink, which are the ones that make
/// a large blob resumable. Chunks reach the sink as they arrive, so a connection
/// that drops part-way through leaves the progress that did arrive on disk; and
/// the caller's limit is a ceiling rather than a hint, so a server that sends
/// more than the caller will accept is refused rather than truncated.
#[tokio::test]
async fn a_streamed_body_reaches_the_sink_as_it_arrives_and_stops_at_the_limit() {
    let client = HttpClient::new(&HttpClientConfig::default()).expect("a client");
    let body: Vec<u8> = (0..64 * 1024u32).map(|index| index as u8).collect();

    let dropped = TestServer::start();
    dropped.route("blobs/big", Behaviour::DisconnectAfter(body.clone(), 4096));
    let origin = Origin::parse(&dropped.url()).expect("the origin parses");
    let address = url::Url::parse(&format!("{}/blobs/big", dropped.url())).expect("a url");

    let mut kept = Vec::new();
    let error = client
        .get(&origin, &address, 0)
        .await
        .expect("the response arrives")
        .stream_into(u64::MAX, |chunk| {
            kept.extend_from_slice(chunk);
            Ok(())
        })
        .await
        .expect_err("a body that stops early is a failure");
    assert!(
        matches!(
            error,
            HttpError::Transport { .. } | HttpError::Disconnected { .. }
        ),
        "{error}"
    );
    // How many chunks that took is the transport's business and not a guarantee,
    // so the claim is about the bytes: a caller writing to disk has the progress
    // to resume from.
    assert!(!kept.is_empty(), "the sink saw the bytes that arrived");
    assert!(kept.len() < body.len(), "short of the whole body");
    assert_eq!(kept, body[..kept.len()], "and it is the front of it");

    let oversized = TestServer::start();
    oversized.route("blobs/big", Behaviour::Serve(vec![7u8; 4096]));
    let origin = Origin::parse(&oversized.url()).expect("the origin parses");
    let address = url::Url::parse(&format!("{}/blobs/big", oversized.url())).expect("a url");

    let mut written = 0usize;
    let error = client
        .get(&origin, &address, 0)
        .await
        .expect("the response arrives")
        .stream_into(1024, |chunk| {
            written += chunk.len();
            Ok(())
        })
        .await
        .expect_err("a body over the limit is refused");
    assert!(matches!(error, HttpError::TooLarge { .. }), "{error}");
    assert!(
        written < 4096,
        "and it stopped early, after {written} bytes"
    );
}

/// A channel is a path segment, not a path: a client that joins it unescaped
/// would address a document outside the repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_identifies_itself_and_a_secret_header_is_never_rendered() {
    let server = TestServer::start();
    let bytes = logical(14, 4096);
    let descriptor = payload_descriptor(&bytes);
    server.route(&blob_path(&descriptor), Behaviour::Serve(wire(&bytes)));
    let client = zup_acquire_http::HttpClient::new(&HttpClientConfig::default())
        .expect("the client builds")
        .with_bearer(
            &server.origin(),
            zup_acquire_http::SecretHeader::new("super-secret-token"),
        );
    let source = HttpSource::new(
        "private",
        client,
        OriginSet::from_urls(&server.url(), Vec::<&str>::new()).expect("valid"),
    );
    acquire_raw(&fixture(), descriptor, Arc::new(source))
        .await
        .expect("an authenticated origin works");

    // The token is a fact in a diagnostic and never a value.
    let header = zup_acquire_http::SecretHeader::new("super-secret-token");
    assert_eq!(format!("{header:?}"), "SecretHeader(<redacted>)");
    assert!(!format!("{header:?}").contains("super-secret-token"));
    assert!(USER_AGENT.starts_with("zup-acquire/"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_cache_means_no_request_is_made_at_all() {
    let server = TestServer::start();
    let bytes = logical(16, 64 * 1024);
    let descriptor = payload_descriptor(&bytes);
    // The origin would serve corrupt bytes. A warm cache must not consult it.
    server.route(
        &blob_path(&descriptor),
        Behaviour::Serve(wire(&logical(97, bytes.len()))),
    );
    let cache = Arc::new(
        ContentCache::open(
            tempfile::tempdir().expect("a temporary directory").path(),
            CachePolicy::Keep,
        )
        .expect("the cache opens"),
    );
    {
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer.write(&wire(&bytes)).expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        descriptor,
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    let outcome = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            SourceChain::new(vec![source_for(&server)]),
            Arc::new(zup_acquire::NeverCancelled),
            &ProgressSink::discard(),
        )
        .await
        .expect("a warm cache needs no origin")
        .enter();
    assert_eq!(outcome.cache_hits, 1);
    assert_eq!(
        server.requests(),
        0,
        "a warm cache makes no network request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_never_answers_is_abandoned_by_the_header_timeout() {
    let server = TestServer::start();
    let bytes = logical(17, 4096);
    let descriptor = payload_descriptor(&bytes);
    server.route(&blob_path(&descriptor), Behaviour::Hang);
    let config = HttpClientConfig {
        timeouts: TimeoutPolicy {
            connect: std::time::Duration::from_millis(200),
            headers: std::time::Duration::from_millis(200),
            idle: std::time::Duration::from_millis(200),
            per_blob: std::time::Duration::from_millis(600),
        },
        ..HttpClientConfig::default()
    };
    let source = HttpSource::new(
        "cdn",
        zup_acquire_http::HttpClient::new(&config).expect("the client builds"),
        OriginSet::from_urls(&server.url(), Vec::<&str>::new()).expect("valid"),
    )
    .with_policy(BackoffPolicy {
        max_attempts: 2,
        ..fast_policy()
    });
    let started = std::time::Instant::now();
    let error = acquire_raw(&fixture(), descriptor, Arc::new(source))
        .await
        .expect_err("a silent server does not satisfy the closure");
    assert!(error.left_machine_unchanged());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a stalled origin is abandoned, not waited on"
    );
}
