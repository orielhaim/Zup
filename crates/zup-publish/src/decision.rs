use zup_core::Sha256Digest;

use crate::plan::ReleaseProduct;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetState {
    Starter,
    Uploaded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAsset {
    pub name: String,
    pub size: Option<u64>,
    pub digest: Option<Sha256Digest>,
    pub state: AssetState,
}

impl RemoteAsset {
    pub fn uploaded(name: impl Into<String>, size: u64, digest: Sha256Digest) -> Self {
        Self {
            name: name.into(),
            size: Some(size),
            digest: Some(digest),
            state: AssetState::Uploaded,
        }
    }

    pub fn starter(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            size: Some(0),
            digest: None,
            state: AssetState::Starter,
        }
    }

    pub fn opaque(name: impl Into<String>, size: Option<u64>) -> Self {
        Self {
            name: name.into(),
            size,
            digest: None,
            state: AssetState::Uploaded,
        }
    }

    pub fn is_starter(&self) -> bool {
        matches!(self.state, AssetState::Starter) || self.size == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetConflict {
    pub name: String,
    pub expected_digest: Sha256Digest,
    pub expected_size: u64,
    pub found_digest: Option<Sha256Digest>,
    pub found_size: Option<u64>,
    pub reason: ConflictReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    DifferentBytes,
    FailedUpload,
    Unverifiable,
}

impl ConflictReason {
    pub const fn is_removable(self) -> bool {
        matches!(self, Self::FailedUpload)
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::DifferentBytes => "the host already holds different bytes under this name",
            Self::FailedUpload => "a previous upload left an empty asset under this name",
            Self::Unverifiable => "the host reports an asset but no digest for it",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetAction {
    Present,
    Upload,
    Replace(AssetConflict),
    Conflict(AssetConflict),
}

impl AssetAction {
    pub const fn uploads(&self) -> bool {
        matches!(self, Self::Upload | Self::Replace(_))
    }

    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }

    pub fn conflict(&self) -> Option<&AssetConflict> {
        match self {
            Self::Replace(conflict) | Self::Conflict(conflict) => Some(conflict),
            Self::Present | Self::Upload => None,
        }
    }
}

pub fn classify(product: &ReleaseProduct, remote: Option<&RemoteAsset>) -> AssetAction {
    let Some(remote) = remote else {
        return AssetAction::Upload;
    };
    if remote.is_starter() {
        return AssetAction::Replace(conflict(product, remote, ConflictReason::FailedUpload));
    }
    let conflict = conflict(product, remote, ConflictReason::Unverifiable);
    match remote.digest {
        Some(digest) if digest == product.digest && remote.size == Some(product.size) => {
            AssetAction::Present
        }
        Some(_) => AssetAction::Conflict(AssetConflict {
            reason: ConflictReason::DifferentBytes,
            ..conflict
        }),
        None => AssetAction::Conflict(conflict),
    }
}

fn conflict(
    product: &ReleaseProduct,
    remote: &RemoteAsset,
    reason: ConflictReason,
) -> AssetConflict {
    AssetConflict {
        name: product.name.clone(),
        expected_digest: product.digest,
        expected_size: product.size,
        found_digest: remote.digest,
        found_size: remote.size,
        reason,
    }
}

pub fn describe_conflict(conflict: &AssetConflict) -> String {
    match conflict.reason {
        ConflictReason::DifferentBytes => format!(
            "`{}`: the release expects sha256:{} ({} bytes) and the host holds sha256:{} ({} bytes)",
            conflict.name,
            conflict.expected_digest.to_hex(),
            conflict.expected_size,
            conflict
                .found_digest
                .map_or("unknown".to_owned(), |digest| digest.to_hex()),
            conflict
                .found_size
                .map_or("unknown".to_owned(), |size| size.to_string())
        ),
        ConflictReason::FailedUpload => {
            format!(
                "`{}`: an earlier upload left an empty asset behind",
                conflict.name
            )
        }
        ConflictReason::Unverifiable => format!(
            "`{}`: the host reports an asset of {} bytes but no digest, so it cannot be \
             compared to the expected sha256:{}",
            conflict.name,
            conflict
                .found_size
                .map_or("unknown".to_owned(), |size| size.to_string()),
            conflict.expected_digest.to_hex()
        ),
    }
}
