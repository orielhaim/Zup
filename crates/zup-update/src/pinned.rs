//! Fetching a file a manifest pinned by URL and digest.
//!
//! This exists for **system prerequisites**, not for application content. The
//! distinction is architectural and load-bearing:
//!
//! ```text
//! shared system prerequisites   ≠   application-owned CAS resources
//! a prerequisite is a .msi or a redistributable the machine shares
//! an application resource is a file an installation's ledger owns
//! ```
//!
//! They share a *transport* without sharing *ownership*. A prerequisite is
//! staged into a quarantine and executed by a package installer; it is not in
//! any installation's closure, so it is not in the content-addressed cache, is
//! not covered by a retention policy, and cannot be swept or restored as if it
//! were application content. What it shares is what was genuinely duplicated:
//! one pooled client, the same redirect policy, the same size ceiling, and a
//! write path that publishes only after the digest matches.
//!
//! Keeping the two apart is why this is its own module rather than an
//! `AcquisitionItem`. Making a prerequisite a cache object would let one
//! installation evict a redistributable another installation depends on.

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

/// Fetch `url` into `destination`, publishing there only if it hashes to
/// `expected`.
///
/// The bytes are written to a temporary sibling and renamed, so the destination
/// never contains unverified content and a caller never has to decide whether a
/// leftover partial is safe to keep. That matters because this writes into a
/// quarantine a later run may find, and a stale partial that looked like content
/// would be a way past the digest check.
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
    if origin.scheme() != "https" && !origin.is_loopback() && !origin.is_local() {
        return Err(PinnedError::Url);
    }
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
    let temporary = parent.join(format!(
        ".zup-pinned-{}.partial",
        zup_core::base64_encode(&rand_suffix())
    ));
    let size = match write_verified(response, &temporary, expected, expected_size, max_size).await {
        Ok(size) => size,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    if let Err(error) = tokio::fs::rename(&temporary, destination).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(PinnedError::Io(error));
    }
    Ok(PinnedDownload {
        path: destination.to_path_buf(),
        size,
        digest: expected,
    })
}

async fn write_verified(
    response: zup_acquire_http::Response,
    temporary: &Path,
    expected: Sha256Digest,
    expected_size: Option<u64>,
    max_size: u64,
) -> Result<u64, PinnedError> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .await?;
    let mut stream = response.into_body().bytes_stream();
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|error| PinnedError::Transport(error.to_string()))?;
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

fn rand_suffix() -> [u8; 8] {
    // A unique-enough temporary name. This is a sibling of a file in a
    // per-installation quarantine, so the requirement is "not the same name as a
    // concurrent run's temporary", not unpredictability.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id() as u128;
    let mixed = nanos ^ (pid << 17);
    (mixed.to_le_bytes()[..8]).try_into().expect("eight bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_that_could_leak_a_credential_is_refused() {
        for hostile in [
            "http://updates.example.com/msi/x.msi",
            "https://user:pw@host/x.msi",
            "https://host/x.msi#frag",
            "not a url",
        ] {
            assert!(
                Origin::parse(hostile).is_err(),
                "{hostile} must not parse into an origin"
            );
        }
        // The only plain-HTTP case is one that does not leave the machine.
        assert!(Origin::parse("http://127.0.0.1:8080/x.msi").is_ok());
        assert!(Origin::parse("https://updates.example.com/x.msi").is_ok());
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
