#![allow(dead_code)]

//! A GitHub that misbehaves in every documented way.
//!
//! The real API's failure modes are the whole reason the publisher's state
//! machine exists, and none of them can be tested against the real API: a
//! `502` that leaves a zero-byte asset behind, a `422` for a duplicate filename,
//! a digest that disagrees with the bytes, a rate limit that arrives as a `403`
//! rather than a `429`. A publisher that has only ever seen a well-behaved host
//! has not been tested at all.
//!
//! So this is a hand-rolled `TcpListener` rather than a framework, for the same
//! reason the acquisition tests do it: every response is exactly what the test
//! says it is, including the ones GitHub documents but a mock framework has no
//! vocabulary for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};

/// What a mock host does when a request arrives.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// A status to answer with instead of the normal handling.
    pub status: Option<u16>,
    /// A `Retry-After` header, in seconds.
    pub retry_after: Option<u64>,
    /// `x-ratelimit-remaining: 0`, which is how GitHub says "slow down" on a 403.
    pub rate_limited: bool,
    /// Fail the next N upload attempts this way, then behave.
    ///
    /// A count rather than a flag, because the interesting case is "the first
    /// attempt fails and leaves a remnant behind, the second succeeds" - which a
    /// boolean cannot express.
    pub upload_failures: u32,
    /// Leave an empty `starter` asset behind when an upload fails.
    pub leaves_starter: bool,
    /// The digest to report for the next upload, when it is not the real one.
    pub report_wrong_digest: bool,
    /// The size to report for the next upload.
    pub report_wrong_size: Option<u64>,
    /// The name to report for the next upload.
    pub rename_to: Option<String>,
    /// Answer `200` to an asset download that asked for a range.
    pub ignore_range: bool,
    /// Answer a range with a `206` whose `Content-Range` names a different range.
    pub wrong_content_range: bool,
    /// The repository is private.
    pub private: bool,
    /// The repository is archived.
    pub archived: bool,
    /// Whether the repository has immutable releases enabled.
    pub immutable_releases: Option<bool>,
    /// Whether a release is immutable.
    pub release_immutable: Option<bool>,
    /// Whether a release is a draft.
    pub draft: bool,
    /// Whether a release exists at all.
    pub present: bool,
}

impl Behaviour {
    /// A well-behaved host.
    pub fn new() -> Self {
        Self {
            present: true,
            draft: true,
            ..Self::default()
        }
    }

    /// A host with no release for the requested tag.
    pub fn absent() -> Self {
        Self {
            present: false,
            ..Self::new()
        }
    }

    /// A host whose release is already published.
    pub fn published() -> Self {
        Self {
            draft: false,
            ..Self::new()
        }
    }
}

/// One asset the mock host holds.
#[derive(Debug, Clone)]
struct Remote {
    id: u64,
    name: String,
    size: u64,
    state: String,
    digest: Option<String>,
    bytes: Vec<u8>,
}

/// A mock GitHub.
pub struct Github {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    requests: Arc<AtomicU64>,
    uploads: Arc<AtomicU64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct State {
    behaviour: Behaviour,
    assets: Vec<Remote>,
    release_id: u64,
    next_asset_id: u64,
    /// Every request line the host saw, for a test that asserts on ordering.
    log: Vec<String>,
}

impl Github {
    /// Start a host with `behaviour`.
    pub fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let address = listener.local_addr().expect("the listener has an address");
        listener
            .set_nonblocking(true)
            .expect("the listener can poll");
        let state = Arc::new(Mutex::new(State {
            release_id: 4242,
            next_asset_id: 9000,
            behaviour,
            ..State::default()
        }));
        let requests = Arc::new(AtomicU64::new(0));
        let uploads = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let state = Arc::clone(&state);
            let requests = Arc::clone(&requests);
            let uploads = Arc::clone(&uploads);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let state = Arc::clone(&state);
                            let requests = Arc::clone(&requests);
                            let uploads = Arc::clone(&uploads);
                            std::thread::spawn(move || {
                                let _ = serve(stream, &state, &requests, &uploads);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            address,
            state,
            requests,
            uploads,
            shutdown,
            handle: Some(handle),
        }
    }

    /// The port the mock host is listening on.
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The API base, as an origin.
    pub fn api_origin(&self) -> zup_acquire_http::Origin {
        zup_acquire_http::Origin::parse(&format!("http://{}/", self.address))
            .expect("a loopback origin parses")
    }

    /// The upload base, as an origin.
    pub fn upload_origin(&self) -> zup_acquire_http::Origin {
        zup_acquire_http::Origin::parse(&format!("http://{}/", self.address))
            .expect("a loopback origin parses")
    }

    /// The address a client is told the repository lives at.
    pub fn repository(&self) -> zup_publish_github::GithubRepository {
        zup_publish_github::GithubRepository::dotcom("acme", "acme")
    }

    /// Change the host's behaviour mid-test.
    pub fn set(&self, behaviour: Behaviour) {
        self.state
            .lock()
            .expect("the state is not poisoned")
            .behaviour = behaviour;
    }

    /// Read the host's behaviour.
    pub fn behaviour(&self) -> Behaviour {
        self.state
            .lock()
            .expect("the state is not poisoned")
            .behaviour
            .clone()
    }

    /// How many requests the host has answered.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// How many upload attempts the host has seen, successful or not.
    pub fn upload_attempts(&self) -> u64 {
        self.uploads.load(Ordering::Relaxed)
    }

    /// The assets the host currently holds, as `(name, size, digest)`.
    pub fn assets(&self) -> Vec<(String, u64, Option<String>)> {
        self.state
            .lock()
            .expect("the state is not poisoned")
            .assets
            .iter()
            .map(|asset| (asset.name.clone(), asset.size, asset.digest.clone()))
            .collect()
    }

    /// The asset names the host currently holds.
    pub fn asset_names(&self) -> Vec<String> {
        self.assets().into_iter().map(|(name, ..)| name).collect()
    }

    /// Every request line the host saw.
    pub fn log(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("the state is not poisoned")
            .log
            .clone()
    }

    /// Put an asset on the host as if it had been uploaded successfully.
    pub fn put(&self, name: &str, bytes: &[u8]) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        let id = state.next_asset_id;
        state.next_asset_id += 1;
        state.assets.push(Remote {
            id,
            name: name.to_owned(),
            size: bytes.len() as u64,
            state: "uploaded".to_owned(),
            digest: Some(format!("sha256:{}", hex(&Sha256::digest(bytes)))),
            bytes: bytes.to_vec(),
        });
    }

    /// Put an empty `starter` asset on the host, as a failed upload leaves behind.
    pub fn put_starter(&self, name: &str) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        let id = state.next_asset_id;
        state.next_asset_id += 1;
        state.assets.push(Remote {
            id,
            name: name.to_owned(),
            size: 0,
            state: "starter".to_owned(),
            digest: None,
            bytes: Vec::new(),
        });
    }
}

impl Drop for Github {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn serve(
    stream: TcpStream,
    state: &Arc<Mutex<State>>,
    requests: &Arc<AtomicU64>,
    uploads: &Arc<AtomicU64>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.eq_ignore_ascii_case("chunked")
            {
                // An upload body is a stream, so there is no length to trust and
                // the chunks are the only thing that says how many bytes arrived.
                // A mock that reads a declared length would see zero and report
                // a wrong digest for a correct upload.
                chunked = true;
            }
        }
    }
    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        body
    };
    requests.fetch_add(1, Ordering::Relaxed);
    let (raw, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    // The route table is written in the shape GitHub's API is documented in,
    // which is without the leading slash the request line always has.
    let path = raw.trim_start_matches('/');
    let mut writer = stream;
    // Log and read the behaviour under the lock, then release it. The arms below
    // take the lock themselves, and a `Mutex` is not reentrant - holding it
    // across a dispatch that locks again is a deadlock rather than a test.
    let behaviour = {
        let mut guard = state.lock().expect("the state is not poisoned");
        guard.log.push(format!("{method} {path}"));
        guard.behaviour.clone()
    };
    if let Some(status) = behaviour.status {
        let message = message_for(status);
        return write_json(
            &mut writer,
            status,
            &behaviour,
            &serde_json::json!({ "message": message }),
        );
    }
    if path.ends_with("/assets") && method == "POST" {
        uploads.fetch_add(1, Ordering::Relaxed);
        return upload(&mut writer, state, query, &body, &behaviour);
    }
    if method == "GET" && path.contains("assets") {
        return asset_list(&mut writer, state);
    }
    if method == "GET" && path.ends_with("/releases") && path.contains("generate-notes") {
        return write_json(
            &mut writer,
            200,
            &behaviour,
            &serde_json::json!({ "body": "## What's Changed\n* a fix" }),
        );
    }
    if method == "GET" && path.starts_with("repos/") && path.matches('/').count() == 2 {
        // `GET /repos/{owner}/{repo}` - and the upload host is the same
        // listener, so the path shape is what distinguishes the two.
        return write_json(
            &mut writer,
            200,
            &behaviour,
            &serde_json::json!({
                "full_name": "acme/acme",
                "private": behaviour.private,
                "archived": behaviour.archived,
                "default_branch": "main",
                "immutable_releases": behaviour.immutable_releases,
            }),
        );
    }
    if method == "GET" && path.contains("releases") {
        if !behaviour.present {
            return write_json(
                &mut writer,
                404,
                &behaviour,
                &serde_json::json!({ "message": "Not Found" }),
            );
        }
        let payload = {
            let guard = state.lock().expect("the state is not poisoned");
            release_json(&guard, &behaviour)
        };
        return write_json(&mut writer, 200, &behaviour, &payload);
    }
    if method == "POST" && path.ends_with("releases") {
        let mut guard = state.lock().expect("the state is not poisoned");
        guard.behaviour.draft = true;
        guard.behaviour.present = true;
        let behaviour = guard.behaviour.clone();
        let payload = release_json(&guard, &behaviour);
        drop(guard);
        return write_json(&mut writer, 201, &behaviour, &payload);
    }
    if method == "PATCH" || method == "POST" {
        let mut guard = state.lock().expect("the state is not poisoned");
        let requested: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        if let Some(draft) = requested.get("draft").and_then(|value| value.as_bool()) {
            guard.behaviour.draft = draft;
        }
        let behaviour = guard.behaviour.clone();
        let payload = release_json(&guard, &behaviour);
        drop(guard);
        return write_json(&mut writer, 200, &behaviour, &payload);
    }
    if method == "DELETE" {
        let mut guard = state.lock().expect("the state is not poisoned");
        let id: u64 = path
            .rsplit('/')
            .next()
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        guard.assets.retain(|asset| asset.id != id);
        let behaviour = guard.behaviour.clone();
        drop(guard);
        return write_status(&mut writer, 204, &behaviour);
    }
    write_json(
        &mut writer,
        404,
        &behaviour,
        &serde_json::json!({ "message": "Not Found" }),
    )
}

/// Read a chunked body to its terminating zero-length chunk.
fn read_chunked(reader: &mut BufReader<TcpStream>) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        if size == 0 {
            // The trailer section, terminated by a blank line.
            let mut trailer = String::new();
            while reader.read_line(&mut trailer)? > 0 {
                if trailer.trim().is_empty() {
                    break;
                }
                trailer.clear();
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    Ok(out)
}

fn release_json(state: &State, behaviour: &Behaviour) -> serde_json::Value {
    let assets: Vec<serde_json::Value> = state
        .assets
        .iter()
        .map(|asset| {
            serde_json::json!({
                "id": asset.id,
                "name": asset.name,
                "size": asset.size,
                "state": asset.state,
                "digest": asset.digest,
                "browser_download_url": format!("http://example.invalid/{}", asset.name),
            })
        })
        .collect();
    serde_json::json!({
        "id": state.release_id,
        "tag_name": "v1.4.0",
        "name": "v1.4.0",
        "body": "notes",
        "draft": behaviour.draft,
        "prerelease": false,
        "immutable": behaviour.release_immutable,
        "html_url": "http://example.invalid/releases/v1.4.0",
        "target_commitish": "main",
        "assets": assets,
    })
}

fn asset_list(writer: &mut TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let state = state.lock().expect("the state is not poisoned");
    let behaviour = state.behaviour.clone();
    let assets: Vec<serde_json::Value> = state
        .assets
        .iter()
        .map(|asset| {
            serde_json::json!({
                "id": asset.id,
                "name": asset.name,
                "size": asset.size,
                "state": asset.state,
                "digest": asset.digest,
                "browser_download_url": format!("http://example.invalid/{}", asset.name),
            })
        })
        .collect();
    write_json(writer, 200, &behaviour, &serde_json::Value::Array(assets))
}

fn upload(
    writer: &mut TcpStream,
    state: &Arc<Mutex<State>>,
    query: &str,
    body: &[u8],
    behaviour: &Behaviour,
) -> std::io::Result<()> {
    let name = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("name="))
        .map(percent_decode)
        .unwrap_or_default();
    if behaviour.upload_failures > 0 {
        let mut state = state.lock().expect("the state is not poisoned");
        state.behaviour.upload_failures -= 1;
        if behaviour.leaves_starter {
            let id = state.next_asset_id;
            state.next_asset_id += 1;
            state.assets.push(Remote {
                id,
                name: name.clone(),
                size: 0,
                state: "starter".to_owned(),
                digest: None,
                bytes: Vec::new(),
            });
        }
        let mut failed = behaviour.clone();
        failed.upload_failures = state.behaviour.upload_failures;
        drop(state);
        return write_json(
            writer,
            502,
            &failed,
            &serde_json::json!({ "message": "Bad Gateway" }),
        );
    }
    let mut state = state.lock().expect("the state is not poisoned");
    let id = state.next_asset_id;
    state.next_asset_id += 1;
    let digest = if behaviour.report_wrong_digest {
        Some(format!("sha256:{}", "0".repeat(64)))
    } else {
        Some(format!("sha256:{}", hex(&Sha256::digest(body))))
    };
    let reported_name = behaviour.rename_to.clone().unwrap_or_else(|| name.clone());
    let reported_size = behaviour.report_wrong_size.unwrap_or(body.len() as u64);
    state.assets.push(Remote {
        id,
        name: reported_name,
        size: reported_size,
        state: "uploaded".to_owned(),
        digest,
        bytes: body.to_vec(),
    });
    let asset = state.assets.last().expect("the asset was just pushed");
    let payload = serde_json::json!({
        "id": asset.id,
        "name": asset.name,
        "size": asset.size,
        "state": asset.state,
        "digest": asset.digest,
        "browser_download_url": format!("http://example.invalid/{}", asset.name),
    });
    let behaviour = state.behaviour.clone();
    write_json(writer, 201, &behaviour, &payload)
}

fn write_json(
    writer: &mut TcpStream,
    status: u16,
    behaviour: &Behaviour,
    payload: &serde_json::Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(payload).expect("the fixture is serializable");
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reason(status),
        body.len()
    );
    if let Some(seconds) = behaviour.retry_after {
        head.push_str(&format!("Retry-After: {seconds}\r\n"));
    }
    if behaviour.rate_limited {
        head.push_str("X-RateLimit-Remaining: 0\r\nX-RateLimit-Resource: core\r\n");
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn write_status(writer: &mut TcpStream, status: u16, behaviour: &Behaviour) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nConnection: close\r\n",
        reason(status)
    );
    if behaviour.rate_limited {
        head.push_str("X-RateLimit-Remaining: 0\r\n");
    }
    if let Some(seconds) = behaviour.retry_after {
        head.push_str(&format!("Retry-After: {seconds}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn message_for(status: u16) -> &'static str {
    match status {
        401 => "Bad credentials",
        403 => "Resource not accessible by integration",
        404 => "Not Found",
        422 => "Validation Failed",
        _ => "Something went wrong",
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Fill a buffer deterministically, for a caller that needs bytes of a size.
pub fn fill(buffer: &mut [u8], seed: u8) {
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
}
