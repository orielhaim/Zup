#![allow(dead_code)]

//! A local origin that misbehaves in every way a real one does.
//!
//! These fixtures are the point of the transport tests. A CDN that behaves well
//! proves nothing; what has to hold is that a CDN that lies about a length,
//! answers a range with the whole object, returns 429 with a `Retry-After`,
//! drops a connection mid-body, or serves corrupt bytes is handled correctly and
//! cheaply.
//!
//! The server is a hand-rolled `TcpListener` rather than a framework, so each
//! response is exactly what the test says it is.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// How one path should be answered.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// Serve these bytes with full range support.
    Serve(Vec<u8>),
    /// Serve these bytes, ignoring `Range` and answering `200`.
    ServeWithoutRange(Vec<u8>),
    /// Answer with a status and no body.
    Status(u16),
    /// Answer with a status, a `Retry-After`, and no body.
    Throttled(u16, u64),
    /// Serve a prefix, then close the connection without finishing.
    DisconnectAfter(Vec<u8>, usize),
    /// Serve bytes that are not what the caller asked for.
    Corrupt(Vec<u8>),
    /// Promise one length and send another.
    WrongContentLength(Vec<u8>),
    /// Answer a range request with a `206` whose `Content-Range` is wrong.
    WrongContentRange(Vec<u8>),
    /// Answer a range request with a `206` and no `Content-Range` at all.
    NoContentRange(Vec<u8>),
    /// Redirect to `location`.
    Redirect(String),
    /// Slow down: pause before each chunk.
    Slow(Vec<u8>, u64),
    /// Never answer at all.
    Hang,
}

/// One local origin.
pub struct TestServer {
    address: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Behaviour>>>,
    requests: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    /// Start a server with no routes. A request for an unrouted path is a 404.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let address = listener.local_addr().expect("the listener has an address");
        listener
            .set_nonblocking(true)
            .expect("the listener can poll");
        let routes: Arc<Mutex<HashMap<String, Behaviour>>> = Arc::new(Mutex::new(HashMap::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let ranges: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_routes = Arc::clone(&routes);
        let thread_requests = Arc::clone(&requests);
        let thread_ranges = Arc::clone(&ranges);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let routes = Arc::clone(&thread_routes);
                        let requests = Arc::clone(&thread_requests);
                        let ranges = Arc::clone(&thread_ranges);
                        std::thread::spawn(move || {
                            let _ = serve(stream, &routes, &requests, &ranges);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            routes,
            requests,
            ranges,
            shutdown,
            handle: Some(handle),
        }
    }

    /// The base URL of this origin.
    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    /// An origin that points at this server.
    pub fn origin(&self) -> zup_acquire_http::Origin {
        zup_acquire_http::Origin::parse(&self.url()).expect("a loopback origin parses")
    }

    /// Route `path` to a behaviour.
    pub fn route(&self, path: &str, behaviour: Behaviour) {
        self.routes
            .lock()
            .expect("the route table is not poisoned")
            .insert(path.trim_start_matches('/').to_owned(), behaviour);
    }

    /// How many requests this server has answered.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// Stop answering, so a client sees a refused connection.
    pub fn server_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Every `Range` header this server has seen, in order.
    pub fn ranges(&self) -> Vec<Option<String>> {
        self.ranges
            .lock()
            .expect("the range log is not poisoned")
            .clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    stream: TcpStream,
    routes: &Arc<Mutex<HashMap<String, Behaviour>>>,
    requests: &Arc<AtomicUsize>,
    ranges: &Arc<Mutex<Vec<Option<String>>>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut range: Option<String> = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_owned());
        }
    }
    requests.fetch_add(1, Ordering::Relaxed);
    ranges
        .lock()
        .expect("the range log is not poisoned")
        .push(range.clone());
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_start_matches('/')
        .to_owned();
    let behaviour = routes
        .lock()
        .expect("the route table is not poisoned")
        .get(&path)
        .cloned()
        .unwrap_or(Behaviour::Status(404));
    let mut writer = stream;
    match behaviour {
        Behaviour::Hang => {
            // Hold the connection open without answering. The client's header
            // timeout is what has to end this.
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        }
        Behaviour::Status(status) => write_status(&mut writer, status, None),
        Behaviour::Throttled(status, seconds) => write_status(
            &mut writer,
            status,
            Some(("Retry-After", seconds.to_string())),
        ),
        Behaviour::Redirect(location) => write_redirect(&mut writer, &location),
        Behaviour::Serve(bytes) => write_body(&mut writer, &bytes, range.as_deref(), true, None),
        Behaviour::ServeWithoutRange(bytes) => write_body(&mut writer, &bytes, None, false, None),
        Behaviour::Corrupt(bytes) => write_body(&mut writer, &bytes, None, false, None),
        Behaviour::WrongContentLength(bytes) => write_body(
            &mut writer,
            &bytes,
            None,
            false,
            Some(bytes.len() as u64 + 17),
        ),
        Behaviour::WrongContentRange(bytes) => {
            write_body(&mut writer, &bytes, range.as_deref(), true, Some(999_999))
        }
        Behaviour::NoContentRange(bytes) => {
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            writer.write_all(head.as_bytes())?;
            writer.write_all(&bytes)?;
            writer.flush()
        }
        Behaviour::Slow(bytes, millis) => {
            // A pause before the body, which is what a throttled origin looks
            // like from the client's side.
            std::thread::sleep(std::time::Duration::from_millis(millis));
            write_body(&mut writer, &bytes, range.as_deref(), true, None)
        }
        Behaviour::DisconnectAfter(bytes, after) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            writer.write_all(head.as_bytes())?;
            writer.write_all(&bytes[..after.min(bytes.len())])?;
            writer.flush()?;
            // Close without the rest of the body. This is what a dropped
            // connection looks like to a client.
            let _ = writer.shutdown(std::net::Shutdown::Both);
            Ok(())
        }
    }
}

fn write_status(
    writer: &mut TcpStream,
    status: u16,
    extra: Option<(&str, String)>,
) -> std::io::Result<()> {
    let reason = match status {
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut head =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n");
    if let Some((name, value)) = extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.flush()
}

fn write_redirect(writer: &mut TcpStream, location: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    writer.write_all(head.as_bytes())?;
    writer.flush()
}

fn write_body(
    writer: &mut TcpStream,
    bytes: &[u8],
    range: Option<&str>,
    honour_range: bool,
    override_total: Option<u64>,
) -> std::io::Result<()> {
    let requested: Option<(usize, Option<usize>)> = if honour_range {
        range.and_then(|value| {
            let value = value.strip_prefix("bytes=")?;
            let (start, end) = value.split_once('-')?;
            let start: usize = start.trim().parse().ok()?;
            let end = if end.trim().is_empty() {
                None
            } else {
                end.trim().parse().ok()
            };
            Some((start, end))
        })
    } else {
        None
    };

    match requested {
        Some((start, end)) if start < bytes.len() => {
            let stop = end
                .map(|end| (end + 1).min(bytes.len()))
                .unwrap_or(bytes.len());
            let slice = &bytes[start..stop];
            let total = override_total.unwrap_or(bytes.len() as u64);
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                slice.len(),
                start,
                stop.saturating_sub(1),
                total
            );
            writer.write_all(head.as_bytes())?;
            writer.write_all(slice)?;
        }
        Some(_) => {
            let head = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            writer.write_all(head.as_bytes())?;
        }
        None => {
            let declared = override_total.unwrap_or(bytes.len() as u64);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            );
            writer.write_all(head.as_bytes())?;
            writer.write_all(bytes)?;
        }
    }
    writer.flush()
}

/// Read a fixed number of bytes, for a caller that wants to fill a pipe.
pub fn fill(buffer: &mut [u8], seed: u8) {
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
}

/// A server preloaded with a set of blobs at their immutable paths.
pub struct TestOrigin {
    server: TestServer,
    /// Path of each blob, so a test can seed a local tree from the same fixture.
    pub paths: std::collections::BTreeMap<zup_core::Sha256Digest, String>,
}

impl TestOrigin {
    /// Serve `blobs` as `(descriptor, wire)` pairs.
    pub fn start(blobs: &[(zup_acquire::ContentDescriptor, Vec<u8>)]) -> Self {
        let server = TestServer::start();
        let mut paths = std::collections::BTreeMap::new();
        for (descriptor, wire) in blobs {
            let path = zup_acquire::WebLayout::blob(&descriptor.digest).to_string();
            server.route(&path, Behaviour::Serve(wire.clone()));
            paths.insert(descriptor.digest, path);
        }
        Self { server, paths }
    }

    /// The base URL of this origin.
    pub fn url(&self) -> String {
        self.server.url()
    }
}

/// Write a blob into a directory in the immutable web layout.
pub fn seed_tree(root: &std::path::Path, descriptor: &zup_acquire::ContentDescriptor, wire: &[u8]) {
    let mut path = root.to_path_buf();
    for segment in zup_acquire::WebLayout::blob(&descriptor.digest)
        .to_string()
        .split('/')
    {
        path.push(segment);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the tree is created");
    }
    std::fs::write(&path, wire).expect("the blob is written");
}

/// Drain a reader, for a caller that must consume a body.
pub fn drain(mut reader: impl Read) -> usize {
    let mut buffer = [0u8; 8192];
    let mut total = 0;
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        total += read;
    }
    total
}
