//! What to do about a file a host already has.
//!
//! Publishing is a retry, and a retry is what has to be safe. The interesting
//! question is never "did my upload return 200" - it is "what does the host
//! actually hold under this name right now", and then "is that the same file".
//!
//! So the decision is a pure function of two things: the product the plan
//! expects, and what the host reports. It has four answers and no I/O, which
//! means the whole idempotency contract is testable without a network.
//!
//! ```text
//! absent              → Upload
//! present, same bytes → Present    (skip; this is what makes a rerun cheap)
//! present, not proven → Replace    (a failed remnant, safe to remove)
//! present, different  → Conflict   (fail; never silently overwrite)
//! ```
//!
//! `Conflict` is the important one. A draft release that already holds a
//! *different* file under a name is a situation a human has to look at: either
//! the plan changed, or something else wrote to this release, and neither is
//! something a publisher should resolve by picking a winner.

use zup_core::Sha256Digest;

use crate::plan::ReleaseProduct;

/// What a host reports about an asset's own state.
///
/// A host that records an upload before the bytes arrive leaves a `starter`
/// entry behind when the upload fails, and that entry is a real asset as far as
/// every listing endpoint is concerned. Treating it as an asset is what produces
/// a release with a zero-byte installer on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetState {
    /// The host accepted the name and is still filling it.
    Starter,
    /// The host has the bytes.
    Uploaded,
}

/// What a host currently holds for one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAsset {
    pub name: String,
    /// The size the host reports.
    pub size: Option<u64>,
    /// The digest the host reports, when it reports one.
    ///
    /// A host that reports no digest has not finished processing the asset, or
    /// does not compute one at all. Either way the file cannot be *proven* to be
    /// the one the plan names, and "cannot be proven" is not "is".
    pub digest: Option<Sha256Digest>,
    pub state: AssetState,
}

impl RemoteAsset {
    /// An asset the host has finished processing.
    pub fn uploaded(name: impl Into<String>, size: u64, digest: Sha256Digest) -> Self {
        Self {
            name: name.into(),
            size: Some(size),
            digest: Some(digest),
            state: AssetState::Uploaded,
        }
    }

    /// An asset the host accepted but has not filled.
    pub fn starter(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            size: Some(0),
            digest: None,
            state: AssetState::Starter,
        }
    }

    /// An asset whose digest the host does not report.
    pub fn opaque(name: impl Into<String>, size: Option<u64>) -> Self {
        Self {
            name: name.into(),
            size,
            digest: None,
            state: AssetState::Uploaded,
        }
    }

    /// Whether this is an empty remnant of a failed upload.
    pub fn is_starter(&self) -> bool {
        matches!(self.state, AssetState::Starter) || self.size == Some(0)
    }
}

/// Where two views of one name disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetConflict {
    pub name: String,
    /// What the plan expects.
    pub expected_digest: Sha256Digest,
    pub expected_size: u64,
    /// What the host reports, when it reports anything.
    pub found_digest: Option<Sha256Digest>,
    pub found_size: Option<u64>,
    /// Why this is not simply a different file.
    pub reason: ConflictReason,
}

/// Why a remote asset under an expected name is not a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The host holds a proven different file.
    DifferentBytes,
    /// The host holds an empty remnant of an upload that did not finish.
    FailedUpload,
    /// The host holds a file but reports no digest, so it cannot be compared.
    Unverifiable,
}

impl ConflictReason {
    /// Whether removing the remote asset and uploading again is safe.
    ///
    /// Safe means: the remote file is provably not the published one, or is
    /// provably empty. A file whose bytes cannot be established is *not* safe to
    /// remove on a guess, which is why `Unverifiable` is a conflict a human
    /// resolves rather than something a publisher cleans up.
    pub const fn is_removable(self) -> bool {
        matches!(self, Self::FailedUpload)
    }

    /// A sentence for a report.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::DifferentBytes => "the host already holds different bytes under this name",
            Self::FailedUpload => "a previous upload left an empty asset under this name",
            Self::Unverifiable => "the host reports an asset but no digest for it",
        }
    }
}

/// What to do about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetAction {
    /// The host already holds exactly these bytes. Skip the upload.
    Present,
    /// The host holds nothing. Upload.
    Upload,
    /// The host holds a remnant that is provably not the published file.
    Replace(AssetConflict),
    /// The host holds something that is not the published file, and a publisher
    /// may not decide what to do about it.
    Conflict(AssetConflict),
}

impl AssetAction {
    /// Whether this action uploads anything.
    pub const fn uploads(&self) -> bool {
        matches!(self, Self::Upload | Self::Replace(_))
    }

    /// Whether this action is a failure a publisher must refuse.
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }

    /// The disagreement, when there is one.
    pub fn conflict(&self) -> Option<&AssetConflict> {
        match self {
            Self::Replace(conflict) | Self::Conflict(conflict) => Some(conflict),
            Self::Present | Self::Upload => None,
        }
    }
}

/// Decide what to do about `product`, given what the host holds for its name.
///
/// The comparison is on `name`, `size`, and SHA-256, and on nothing else. In
/// particular a successful HTTP upload is not evidence: it is a status code
/// about a request, and the only statement about the bytes is a digest both ends
/// computed.
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

/// A sentence describing a conflict, for a human and for a CI log.
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
