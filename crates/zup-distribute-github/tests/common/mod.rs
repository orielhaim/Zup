#![allow(dead_code)]

//! A release origin that answers assets in every way a real one might.
//!
//! The content source has to be correct against all three: a host that honours
//! ranges, a host that answers `200` and sends the whole object anyway, and a
//! host that answers `206` with a range that does not line up. Only the first is
//! an optimisation, and the second is the documented case GitHub does not promise
//! — so the fixture has to be able to be all three.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Everything a connection handler needs, shared with the accept loop.
#[derive(Clone)]
struct Host {
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
    honour: Arc<AtomicBool>,
    wrong: Arc<AtomicBool>,
    truncate: Arc<AtomicBool>,
    truncated: Arc<Mutex<BTreeSet<String>>>,
    requests: Arc<AtomicU64>,
    served: Arc<AtomicU64>,
}

/// A local origin serving release assets.
pub struct Origin {
    address: SocketAddr,
    host: Host,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Origin {
    /// A host that answers a range request correctly.
    pub fn start(objects: BTreeMap<String, Vec<u8>>) -> Self {
        Self::with_options(objects, true, false)
    }

    /// A host with the three answers spelled out.
    pub fn with_options(objects: BTreeMap<String, Vec<u8>>, honour: bool, wrong: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let address = listener.local_addr().expect("the listener has an address");
        listener
            .set_nonblocking(true)
            .expect("the listener can poll");
        let host = Host {
            objects: Arc::new(Mutex::new(objects)),
            ranges: Arc::new(Mutex::new(Vec::new())),
            honour: Arc::new(AtomicBool::new(honour)),
            wrong: Arc::new(AtomicBool::new(wrong)),
            truncate: Arc::new(AtomicBool::new(false)),
            truncated: Arc::new(Mutex::new(BTreeSet::new())),
            requests: Arc::new(AtomicU64::new(0)),
            served: Arc::new(AtomicU64::new(0)),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = {
            let accepting = host.clone();
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let host = accepting.clone();
                            std::thread::spawn(move || {
                                let _ = serve(stream, &host);
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
            host,
            shutdown,
            handle: Some(handle),
        }
    }

    /// Cut the connection partway through the first response for each asset.
    ///
    /// This is what a dropped connection looks like to a client: headers that
    /// promise a length, some of the body, then nothing. Once per asset, so the
    /// retry succeeds and the two attempts can be told apart.
    pub fn truncate_bodies(&self) {
        self.host.truncate.store(true, Ordering::Relaxed);
    }

    /// Add or replace an asset.
    pub fn put(&self, name: &str, bytes: &[u8]) {
        self.host
            .objects
            .lock()
            .expect("the objects are not poisoned")
            .insert(name.to_owned(), bytes.to_vec());
    }

    /// Remove an asset, so a missing shard is a `404`.
    pub fn remove(&self, name: &str) {
        self.host
            .objects
            .lock()
            .expect("the objects are not poisoned")
            .remove(name);
    }

    /// The address the origin listens on.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// A repository whose download addresses are this host.
    ///
    /// The hostname is the one thing `ReleaseLayout` cannot be asked to vary, and
    /// it is exactly the value a test must not be allowed to fake: a layout that
    /// addressed `github.com` while the bytes came from loopback would exercise
    /// nothing. So the fixture builds a real host value pointed at itself.
    pub fn repository(&self) -> zup_publish_github::GithubRepository {
        let base = format!("http://{}/", self.address);
        zup_publish_github::GithubRepository::new(
            zup_publish_github::GithubHost {
                host: self.address.ip().to_string(),
                api_base: base.clone(),
                upload_base: base.clone(),
                web_base: base,
                dotcom: false,
            },
            "acme",
            "acme",
        )
    }

    /// A layout that addresses this host under a pinned tag.
    pub fn pinned(&self) -> zup_distribute_github::ReleaseLayout {
        zup_distribute_github::ReleaseLayout::pinned(self.repository(), "v1.4.0")
    }

    /// A layout that addresses this host under the stable alias.
    pub fn latest(&self) -> zup_distribute_github::ReleaseLayout {
        zup_distribute_github::ReleaseLayout::latest(self.repository())
    }

    /// Every `Range` header the host has seen.
    pub fn ranges(&self) -> Vec<Option<String>> {
        self.host
            .ranges
            .lock()
            .expect("the range log is not poisoned")
            .clone()
    }

    /// How many requests the host has answered.
    pub fn requests(&self) -> u64 {
        self.host.requests.load(Ordering::Relaxed)
    }

    /// How many bytes the host has sent.
    pub fn served(&self) -> u64 {
        self.host.served.load(Ordering::Relaxed)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(stream: TcpStream, host: &Host) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let mut range = None;
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
    host.requests.fetch_add(1, Ordering::Relaxed);
    host.ranges
        .lock()
        .expect("the range log is not poisoned")
        .push(range.clone());
    // The object's name is the last path segment, so a fixture can be addressed
    // by a stable tag, by the `latest` alias, or by a redirect target, without
    // the origin caring which shape it was asked for.
    let name = target
        .split(['/', '?'])
        .next_back()
        .unwrap_or("")
        .to_owned();
    let bytes = {
        let objects = host.objects.lock().expect("the objects are not poisoned");
        objects.get(&name).cloned()
    };
    let mut writer = stream;
    let Some(bytes) = bytes else {
        writer.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return writer.flush();
    };
    // A host that honours a range serves exactly the bytes it was asked for and
    // says so. A host that does not, and a host that answers with a range that is
    // not the one it was asked for, are the two shapes a client has to survive.
    let requested = range.as_deref().and_then(|value| {
        let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
        Some((
            start.trim().parse::<usize>().ok()?,
            end.trim().parse::<usize>().ok()?,
        ))
    });
    // Whether this response will be cut short, decided once per asset so the
    // retry can succeed.
    let cut = host.truncate.load(Ordering::Relaxed)
        && host
            .truncated
            .lock()
            .expect("the truncation log is not poisoned")
            .insert(name.clone());
    let body = match requested {
        Some((start, end)) if host.honour.load(Ordering::Relaxed) && start < bytes.len() => {
            let stop = end.min(bytes.len() - 1);
            let slice = &bytes[start..=stop];
            let (from, to) = if host.wrong.load(Ordering::Relaxed) {
                (start + 1, stop)
            } else {
                (start, stop)
            };
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                slice.len(),
                from,
                to,
                bytes.len()
            );
            writer.write_all(head.as_bytes())?;
            slice
        }
        _ => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            writer.write_all(head.as_bytes())?;
            &bytes[..]
        }
    };
    if cut {
        // Headers promise `body.len()`; fewer bytes arrive. The client sees an
        // incomplete body, which is what a dropped connection looks like.
        let partial = body.len().min(TRUNCATE_AFTER);
        writer.write_all(&body[..partial])?;
        host.served.fetch_add(partial as u64, Ordering::Relaxed);
        // Closing without flushing the rest is the point: the peer sees a short
        // read, not a clean end.
        return writer.flush();
    }
    writer.write_all(body)?;
    host.served.fetch_add(body.len() as u64, Ordering::Relaxed);
    writer.flush()
}

/// How many bytes a truncated response delivers.
const TRUNCATE_AFTER: usize = 1024;

/// Fill a buffer deterministically, so a fixture is reproducible.
pub fn fill(buffer: &mut [u8], seed: u8) {
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
}

/// One blob's three numbers, and its bytes.
#[derive(Clone)]
pub struct Blob {
    pub digest: zup_core::Sha256Digest,
    pub bytes: Vec<u8>,
    pub compressed: Vec<u8>,
}

/// A set of blobs, ready to be packed.
///
/// Compressible but not trivially so, because a fixture where every blob
/// compresses to nothing would make the sharding and range tests measure
/// nothing.
pub fn blobs(count: usize, size: usize) -> Vec<Blob> {
    blobs_from(count, size, 1)
}

/// A set of blobs that share no bytes with a set numbered from a different seed.
///
/// Two fixtures built with the same seed are the same content, and a test that
/// means to compare "mine" against "theirs" has to say so with content rather than
/// with an assumption about how the generator numbers its blobs.
pub fn blobs_from(count: usize, size: usize, seed: u8) -> Vec<Blob> {
    (0..count)
        .map(|index| {
            let mut bytes = vec![0u8; size];
            fill(&mut bytes, seed.wrapping_add(index as u8));
            // Half the blob is random and half is repeated, which is roughly what
            // an installer's payload looks like and keeps the compressed size
            // meaningfully between zero and the logical size.
            let first = bytes[0];
            for slot in bytes[size / 2..].iter_mut() {
                *slot = first;
            }
            let compressed =
                zstd::stream::encode_all(bytes.as_slice(), 3).expect("a fixture blob compresses");
            Blob {
                digest: zup_core::hash_bytes(&bytes),
                bytes,
                compressed,
            }
        })
        .collect()
}

/// The authenticated catalog entry for one of `blobs`.
pub fn catalog_entry(blob: &Blob) -> zup_acquire::CatalogEntry {
    zup_acquire::CatalogEntry::compressed(
        blob.digest,
        blob.compressed.len() as u64,
        blob.bytes.len() as u64,
    )
}

/// The acquisition descriptor for one of `blobs`.
pub fn descriptor(blob: &Blob) -> zup_acquire::ContentDescriptor {
    catalog_entry(blob).descriptor(zup_acquire::ContentKind::Payload)
}
