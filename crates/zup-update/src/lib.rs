//! TUF backed update discovery and verified target downloads.

use std::path::{Path, PathBuf};

use futures_util::TryStreamExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tough::{ExpirationEnforcement, Limits, RepositoryLoader, TargetName};
use url::Url;
use zup_core::{Sha256Digest, UpdateConfig};

const DESCRIPTOR_LIMIT: u64 = 64 * 1024;
const INSTALLER_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelDescriptor {
    pub schema: u32,
    pub app_id: String,
    pub channel: String,
    pub version: Version,
    pub platform: String,
    pub architecture: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate {
        current: Version,
    },
    UpdateAvailable {
        current: Version,
        available: Version,
        target: String,
    },
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("invalid update repository URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("TUF repository verification failed: {0}")]
    Tuf(#[source] Box<tough::error::Error>),
    #[error("update target `{0}` is missing")]
    MissingTarget(String),
    #[error("invalid channel descriptor: {0}")]
    Descriptor(String),
    #[error("update target exceeded its size limit")]
    TooLarge,
    #[error("update target stream failed: {0}")]
    Stream(String),
    #[error("update I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("update descriptor does not match installed application, channel, or platform")]
    Mismatch,
    #[error("target name is invalid")]
    TargetName,
    #[error("pinned artifact digest mismatch")]
    PinnedDigestMismatch,
    #[error("pinned artifact size mismatch")]
    PinnedSizeMismatch,
    #[error("pinned artifact HTTP status was not successful: {0}")]
    PinnedHttpStatus(u16),
}

impl From<tough::error::Error> for UpdateError {
    fn from(error: tough::error::Error) -> Self {
        Self::Tuf(Box::new(error))
    }
}

pub struct Client<'a> {
    config: &'a UpdateConfig,
    app_id: &'a str,
    state: &'a Path,
}

impl<'a> Client<'a> {
    pub fn new(config: &'a UpdateConfig, app_id: &'a str, state_root: &'a Path) -> Self {
        Self {
            config,
            app_id,
            state: state_root,
        }
    }

    async fn repository(&self) -> Result<tough::Repository, UpdateError> {
        let base = Url::parse(&self.config.repository)?;
        if !matches!(base.scheme(), "https" | "http" | "file")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(UpdateError::Descriptor(
                "repository must be an HTTP(S) or file URL without credentials".into(),
            ));
        }
        let mut base = base;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let metadata = base.join("metadata/")?;
        let targets = base.join("targets/")?;
        let datastore = self
            .state
            .join("updates")
            .join(self.app_id)
            .join(&self.config.channel)
            .join("tuf");
        tokio::fs::create_dir_all(&datastore).await?;
        let loader = RepositoryLoader::new(&self.config.trusted_root, metadata, targets)
            .datastore(datastore)
            .expiration_enforcement(ExpirationEnforcement::Safe)
            .limits(Limits {
                max_root_size: 1024 * 1024,
                max_targets_size: 10 * 1024 * 1024,
                max_timestamp_size: 1024 * 1024,
                max_snapshot_size: 1024 * 1024,
                max_root_updates: 32,
            });
        let loader = if base.scheme() == "file" {
            loader.transport(tough::FilesystemTransport)
        } else {
            loader
        };
        Ok(loader.load().await?)
    }

    async fn descriptor(&self) -> Result<(tough::Repository, ChannelDescriptor), UpdateError> {
        let repo = self.repository().await?;
        let name = format!("channels/{}.json", self.config.channel);
        let descriptor: ChannelDescriptor = read_verified(&repo, &name, DESCRIPTOR_LIMIT)
            .await
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|e| UpdateError::Descriptor(e.to_string()))
            })?;
        if descriptor.schema != 1
            || descriptor.app_id != self.app_id
            || descriptor.channel != self.config.channel
            || descriptor.platform != "windows"
            || descriptor.architecture != std::env::consts::ARCH
        {
            return Err(UpdateError::Mismatch);
        }
        let expected_prefix = format!(
            "artifacts/{}/windows-{}/",
            descriptor.version, descriptor.architecture
        );
        let basename = descriptor
            .target
            .strip_prefix(&expected_prefix)
            .unwrap_or("");
        if basename.is_empty()
            || basename.contains('/')
            || basename.contains('\\')
            || !basename.ends_with("Setup.exe")
        {
            return Err(UpdateError::Mismatch);
        }
        Ok((repo, descriptor))
    }

    pub async fn check(&self, current: &Version) -> Result<CheckResult, UpdateError> {
        let (_repo, descriptor) = self.descriptor().await?;
        if descriptor.version <= *current {
            return Ok(CheckResult::UpToDate {
                current: current.clone(),
            });
        }
        Ok(CheckResult::UpdateAvailable {
            current: current.clone(),
            available: descriptor.version,
            target: descriptor.target,
        })
    }

    pub async fn download(&self, target: &str, quarantine: &Path) -> Result<PathBuf, UpdateError> {
        let (repo, descriptor) = self.descriptor().await?;
        if target != descriptor.target {
            return Err(UpdateError::Mismatch);
        }
        let verified = download_verified(&repo, target, INSTALLER_LIMIT, quarantine).await?;
        Ok(verified)
    }
}

async fn read_verified(
    repo: &tough::Repository,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, UpdateError> {
    let name = name
        .parse::<TargetName>()
        .map_err(|_| UpdateError::TargetName)?;
    let mut stream = repo
        .read_target(&name)
        .await?
        .ok_or_else(|| UpdateError::MissingTarget(name.raw().to_owned()))?;
    let mut out = Vec::new();
    while let Some(bytes) = stream
        .try_next()
        .await
        .map_err(|e| UpdateError::Stream(e.to_string()))?
    {
        if out.len() as u64 + bytes.len() as u64 > limit {
            return Err(UpdateError::TooLarge);
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

async fn download_verified(
    repo: &tough::Repository,
    name: &str,
    limit: u64,
    quarantine: &Path,
) -> Result<PathBuf, UpdateError> {
    let name = name
        .parse::<TargetName>()
        .map_err(|_| UpdateError::TargetName)?;
    let mut stream = repo
        .read_target(&name)
        .await?
        .ok_or_else(|| UpdateError::MissingTarget(name.raw().to_owned()))?;
    let parent = quarantine
        .parent()
        .ok_or_else(|| UpdateError::Descriptor("quarantine path must have parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".zup-{}.partial", uuid::Uuid::now_v7()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await?;
    let result = async {
        let mut size = 0u64;
        while let Some(bytes) = stream
            .try_next()
            .await
            .map_err(|e| UpdateError::Stream(e.to_string()))?
        {
            size = size
                .checked_add(bytes.len() as u64)
                .ok_or(UpdateError::TooLarge)?;
            if size > limit {
                return Err(UpdateError::TooLarge);
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        file.sync_all().await?;
        Ok::<_, UpdateError>(())
    }
    .await;
    drop(file);
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&temporary, quarantine).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(UpdateError::Io(error));
    }
    Ok(quarantine.to_path_buf())
}

pub async fn download_pinned(
    url: &str,
    expected_digest: Sha256Digest,
    expected_size: Option<u64>,
    destination: &Path,
    max_size: u64,
) -> Result<PathBuf, UpdateError> {
    if expected_size.is_some_and(|size| size > max_size) {
        return Err(UpdateError::PinnedSizeMismatch);
    }
    let parsed = Url::parse(url)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(UpdateError::Descriptor(
            "pinned artifact URL must be HTTPS without credentials or fragments".into(),
        ));
    }
    let redirect_host = parsed.host_str().map(str::to_owned);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5
                || attempt.url().scheme() != "https"
                || attempt.url().host_str() != redirect_host.as_deref()
            {
                attempt.error("pinned artifact redirect is not permitted")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(10 * 60))
        .build()
        .map_err(|error| UpdateError::Stream(error.to_string()))?;
    let response = client
        .get(parsed.clone())
        .send()
        .await
        .map_err(|error| UpdateError::Stream(error.to_string()))?;
    if response.url().scheme() != "https"
        || response.url().host_str() != parsed.host_str()
        || !response.status().is_success()
    {
        return Err(UpdateError::PinnedHttpStatus(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_size || expected_size.is_some_and(|size| length != size))
    {
        return Err(UpdateError::PinnedSizeMismatch);
    }
    let parent = destination
        .parent()
        .ok_or_else(|| UpdateError::Descriptor("pinned artifact path has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".zup-pinned-{}.partial", uuid::Uuid::now_v7()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await?;
    let result = async {
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        use tokio::io::AsyncWriteExt;
        while let Some(bytes) = stream
            .try_next()
            .await
            .map_err(|error| UpdateError::Stream(error.to_string()))?
        {
            size = size
                .checked_add(bytes.len() as u64)
                .ok_or(UpdateError::TooLarge)?;
            if size > max_size {
                return Err(UpdateError::TooLarge);
            }
            if expected_size.is_some_and(|expected| size > expected) {
                return Err(UpdateError::PinnedSizeMismatch);
            }
            hasher.update(&bytes);
            file.write_all(&bytes).await?;
        }
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(UpdateError::PinnedSizeMismatch);
        }
        if Sha256Digest::from_bytes(hasher.finalize().into()) != expected_digest {
            return Err(UpdateError::PinnedDigestMismatch);
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok::<_, UpdateError>(())
    }
    .await;
    drop(file);
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    if tokio::fs::rename(&temporary, destination).await.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(UpdateError::Io(std::io::Error::other(
            "failed to publish pinned artifact",
        )));
    }
    Ok(destination.to_path_buf())
}
