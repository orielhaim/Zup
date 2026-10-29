//! Digest-pinned system prerequisites, staged separately from application-owned content.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use zup_acquire_http::{HttpClient, HttpClientConfig, Origin};
use zup_core::Sha256Digest;

/// Redirect hops a prerequisite fetch may follow.
pub const MAX_REDIRECTS: usize = 5;

/// A file fetched by URL and proved by digest before use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedDownload {
    /// Where the verified bytes are.
    pub path: PathBuf,
    pub size: u64,
    pub digest: Sha256Digest,
}

/// Why a pinned download could not be verified.
#[derive(Debug, thiserror::Error)]
pub enum PinnedError {
    #[error(
        "a pinned artifact URL must be HTTPS, a local file, or a loopback address, without credentials or a fragment"
    )]
    Url,
    #[error("a pinned artifact exceeds its size limit of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("a pinned artifact is {found} bytes; the manifest declares {expected:?}")]
    Size { expected: Option<u64>, found: u64 },
    #[error("a pinned artifact hashed to {found}, not {expected}")]
    Digest {
        expected: Sha256Digest,
        found: Sha256Digest,
    },
    #[error("a pinned artifact returned HTTP {status}")]
    Status { status: u16 },
    #[error("a pinned artifact transfer failed: {0}")]
    Transport(String),
    #[error("a pinned artifact I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Publish at `destination` only after its size and digest have been verified.
pub async fn fetch_pinned(
    url: &str,
    expected: Sha256Digest,
    expected_size: Option<u64>,
    destination: &Path,
    max_size: u64,
) -> Result<PinnedDownload, PinnedError> {
    if expected_size.is_some_and(|size| size > max_size) {
        return Err(PinnedError::TooLarge { limit: max_size });
    }
    let address = url::Url::parse(url).map_err(|_| PinnedError::Url)?;
    if !address.username().is_empty()
        || address.password().is_some()
        || address.fragment().is_some()
    {
        return Err(PinnedError::Url);
    }
    let origin = Origin::site(&address).map_err(|_| PinnedError::Url)?;
    let client = HttpClient::new(&HttpClientConfig {
        timeouts: zup_acquire_http::TimeoutPolicy {
            per_blob: Duration::from_secs(10 * 60),
            ..zup_acquire_http::TimeoutPolicy::default()
        },
        ..HttpClientConfig::default()
    })
    .map_err(|error| PinnedError::Transport(error.to_string()))?;

    let response = client
        .get(&origin, &address, MAX_REDIRECTS)
        .await
        .map_err(|error| PinnedError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        return Err(PinnedError::Status {
            status: response.status().as_u16(),
        });
    }
    if response
        .declared_length()
        .is_some_and(|length| length > max_size || expected_size.is_some_and(|size| length != size))
    {
        return Err(PinnedError::TooLarge { limit: max_size });
    }

    let parent = destination
        .parent()
        .ok_or_else(|| PinnedError::Transport("the destination has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let (file, temporary) = tempfile::NamedTempFile::new_in(parent)?.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let size = write_verified(response, &mut file, expected, expected_size, max_size).await?;
    drop(file);
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(PinnedDownload {
        path: destination.to_path_buf(),
        size,
        digest: expected,
    })
}

async fn write_verified(
    response: zup_acquire_http::Response,
    file: &mut tokio::fs::File,
    expected: Sha256Digest,
    expected_size: Option<u64>,
    max_size: u64,
) -> Result<u64, PinnedError> {
    use tokio::io::AsyncWriteExt;
    let mut stream = response.into_body().bytes_stream();
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk =
            chunk.map_err(|error| PinnedError::Transport(error.without_url().to_string()))?;
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or(PinnedError::TooLarge { limit: max_size })?;
        if size > max_size {
            return Err(PinnedError::TooLarge { limit: max_size });
        }
        if expected_size.is_some_and(|declared| size > declared) {
            return Err(PinnedError::Size {
                expected: expected_size,
                found: size,
            });
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if expected_size.is_some_and(|declared| declared != size) {
        return Err(PinnedError::Size {
            expected: expected_size,
            found: size,
        });
    }
    let found = Sha256Digest::from_bytes(hasher.finalize().into());
    if found != expected {
        return Err(PinnedError::Digest { expected, found });
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::verified(true, 1024, true)]
    #[case::corrupt(false, 1024, false)]
    #[case::oversized(true, 2, false)]
    #[tokio::test]
    async fn verified_publication_preserves_the_destination_on_failure(
        #[case] correct_digest: bool,
        #[case] limit: u64,
        #[case] succeeds: bool,
    ) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload",
                )
                .expect("response");
        });
        let directory = tempfile::tempdir().expect("directory");
        let destination = directory.path().join("prerequisite");
        std::fs::write(&destination, b"existing").expect("existing destination");
        let expected = zup_core::hash_bytes(if correct_digest {
            b"payload"
        } else {
            b"corrupt"
        });
        let result = fetch_pinned(
            &format!("http://{address}/artifact"),
            expected,
            None,
            &destination,
            limit,
        )
        .await;
        server.join().expect("server");
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(
            std::fs::read(destination).expect("destination"),
            if succeeds {
                b"payload".as_slice()
            } else {
                b"existing".as_slice()
            }
        );
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("entries")
                .count(),
            1
        );
    }

    #[test]
    fn a_declared_size_beyond_the_limit_is_refused_before_a_request() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let destination = std::env::temp_dir().join("zup-pinned-size-test.msi");
        let error = runtime
            .block_on(fetch_pinned(
                "https://updates.example.com/x.msi",
                Sha256Digest::from_bytes([0; 32]),
                Some(4096),
                &destination,
                1024,
            ))
            .expect_err("refused before any request");
        assert!(
            matches!(error, PinnedError::TooLarge { limit: 1024 }),
            "{error}"
        );
        assert!(!destination.exists());
    }
}
