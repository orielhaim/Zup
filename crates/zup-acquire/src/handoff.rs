//! Untrusted release locators and acquisition progress passed between installer processes.
//! Runtime-image and content hashes establish consistency, not TUF authorization.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::{AppId, Sha256Digest, Target};

/// Current handoff schema.
pub const HANDOFF_SCHEMA: u32 = 1;

/// Bound on the serialized handoff.
///
/// It is a few hundred bytes. A larger document is not a handoff any more; it is
/// an attempt to smuggle state past the boundary.
pub const MAX_HANDOFF_BYTES: u64 = 64 * 1024;

/// What the native runtime is being asked to do.
///
/// This is a request, not a grant. The runtime still runs the lifecycle rules:
/// an install of something already installed is refused, a downgrade is refused,
/// and a repair only ever restores committed ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    Cleanup {
        keep: u16,
    },
    /// A first install from an authenticated release.
    Install,
    /// A move to a newer release.
    Upgrade,
    Downgrade,
    Rollback {
        revision: zup_core::ActivationRevision,
    },
    /// A different component selection of the same release.
    Modify,
    /// Restoring committed ownership after drift.
    Repair,
    Uninstall,
}

impl HandoffMode {
    /// The lifecycle verb a command line uses.
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Cleanup { .. } => "cleanup",
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::Rollback { .. } => "rollback",
            Self::Modify => "modify",
            Self::Repair => "repair",
            Self::Uninstall => "uninstall",
        }
    }
}

/// What the acquisition that preceded the handoff already achieved.
///
/// This is presentation, and it is stated as such: it exists so a progress bar
/// can start at 63% rather than at zero, and it is not an input to any
/// decision. The runtime recomputes the estimate from the authenticated catalog
/// and the cache it can see, and if the two disagree the recomputed one wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    /// Wire bytes the closure names in total.
    pub total_bytes: u64,
    /// Wire bytes already present and verified when the session started.
    pub cached_bytes: u64,
    /// Blobs the session found already present.
    pub cache_hits: u64,
    /// Blobs the session fetched.
    pub downloaded_items: u64,
    /// Wall-clock milliseconds the acquisition took.
    pub elapsed_ms: u64,
}

impl SessionSummary {
    /// A summary for a closure that was entirely present.
    pub const fn warm(total_bytes: u64, cached_bytes: u64, cache_hits: u64) -> Self {
        Self {
            total_bytes,
            cached_bytes,
            cache_hits,
            downloaded_items: 0,
            elapsed_ms: 0,
        }
    }

    /// The line a frontend shows to explain where its progress bar starts.
    pub fn resume_line(&self) -> String {
        if self.total_bytes == 0 {
            return String::new();
        }
        format!(
            "resuming at {} of {}",
            crate::plan::format_bytes(self.cached_bytes),
            crate::plan::format_bytes(self.total_bytes)
        )
    }
}

impl std::fmt::Display for SessionSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} cached · {} fetched · {cache_hits} hits in {elapsed_ms}ms",
            crate::plan::format_bytes(self.cached_bytes),
            crate::plan::format_bytes(self.total_bytes.saturating_sub(self.cached_bytes)),
            cache_hits = self.cache_hits,
            elapsed_ms = self.elapsed_ms,
        )
    }
}

/// The authenticated binding between a bootstrapper and a native runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHandoff {
    pub repository: String,
    pub pin: zup_core::ReleasePin,
    pub schema: u32,
    pub app_id: AppId,
    /// The release graph, identified by its own recomputed fingerprint.
    pub release: Sha256Digest,
    /// The digest of the release document's *bytes*, which is how a content cache
    /// is addressed.
    ///
    pub document: Sha256Digest,
    /// The content catalog that release names.
    pub catalog: Sha256Digest,
    /// The variant chosen for this host.
    pub variant: String,
    /// The variant descriptor: the install plan the runtime is about to act on.
    pub manifest: Sha256Digest,
    /// The native runtime image.
    ///
    pub runtime: Sha256Digest,
    pub target: Target,
    /// `gui` or `console`, which decides what the user sees.
    pub frontend: String,
    pub mode: HandoffMode,
    /// The scope the installation is being created in.
    pub scope: String,
    /// What the previous process achieved, for presentation only.
    pub session: SessionSummary,
    /// The components the install is for, as the plan's own identifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
}

/// Why a handoff could not describe a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandoffError {
    #[error("handoff repository locator is empty or exceeds 2048 bytes")]
    Repository,
    #[error("invalid handoff release pin: {0}")]
    Pin(&'static str),
    #[error("unsupported handoff schema")]
    Schema,
    #[error("a handoff names no version-independent scope")]
    Scope,
    #[error("a handoff names an invalid variant: {0}")]
    Variant(&'static str),
    #[error("a handoff exceeds its size bound")]
    TooLarge,
}

impl RuntimeHandoff {
    /// Encode as canonical JSON, bounded.
    pub fn encode(&self) -> Result<Vec<u8>, HandoffError> {
        let bytes = serde_json::to_vec(self).map_err(|_| HandoffError::Schema)?;
        if bytes.len() as u64 > MAX_HANDOFF_BYTES {
            return Err(HandoffError::TooLarge);
        }
        Ok(bytes)
    }

    /// Parse a handoff, enforcing its bound and its shape.
    pub fn parse(bytes: &[u8]) -> Result<Self, HandoffError> {
        if bytes.len() as u64 > MAX_HANDOFF_BYTES {
            return Err(HandoffError::TooLarge);
        }
        let handoff: Self = serde_json::from_slice(bytes).map_err(|_| HandoffError::Schema)?;
        handoff.validate()?;
        Ok(handoff)
    }

    /// Reject a handoff whose names could escape a directory or a URL.
    pub fn validate(&self) -> Result<(), HandoffError> {
        if self.schema != HANDOFF_SCHEMA {
            return Err(HandoffError::Schema);
        }
        if self.repository.is_empty() || self.repository.len() > 2048 {
            return Err(HandoffError::Repository);
        }
        match &self.pin {
            zup_core::ReleasePin::Release { document, .. } => {
                if *document != self.document {
                    return Err(HandoffError::Pin("release pin names a different document"));
                }
            }
            zup_core::ReleasePin::Channel { channel } => {
                crate::layout::release_path(channel).map_err(HandoffError::Pin)?;
            }
        }
        crate::layout::check_segment(&self.variant).map_err(HandoffError::Variant)?;
        if !matches!(self.scope.as_str(), "user" | "machine") {
            return Err(HandoffError::Scope);
        }
        for component in &self.components {
            crate::layout::check_segment(component).map_err(HandoffError::Variant)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Sha256Digest {
        Sha256Digest::from_bytes(
            Sha256::digest(serde_json::to_vec(self).expect("handoff serializes")).into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff() -> RuntimeHandoff {
        RuntimeHandoff {
            repository: "https://example.invalid".into(),
            pin: zup_core::ReleasePin::Channel {
                channel: "stable".into(),
            },
            schema: HANDOFF_SCHEMA,
            app_id: AppId::new("com.example.app").expect("valid app id"),
            release: Sha256Digest::from_bytes([1; 32]),
            document: Sha256Digest::from_bytes([5; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "windows-x64".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: Sha256Digest::from_bytes([4; 32]),
            target: Target::parse("windows-x64").expect("valid triple"),
            frontend: "gui".to_owned(),
            mode: HandoffMode::Install,
            scope: "user".to_owned(),
            session: SessionSummary {
                total_bytes: 1000,
                cached_bytes: 400,
                cache_hits: 3,
                downloaded_items: 2,
                elapsed_ms: 900,
            },
            components: vec!["core".to_owned()],
        }
    }

    #[test]
    fn the_fingerprint_covers_every_field() {
        let one = handoff();
        let baseline = one.digest();
        let mut changed = one.clone();
        changed.release = Sha256Digest::from_bytes([9; 32]);
        assert_ne!(changed.digest(), baseline, "release is bound");
        let mut changed = one.clone();
        changed.document = Sha256Digest::from_bytes([9; 32]);
        assert_ne!(changed.digest(), baseline, "the cache key is bound too");
        let mut changed = one.clone();
        changed.variant = "windows-arm64".to_owned();
        assert_ne!(changed.digest(), baseline, "variant is bound");
        let mut changed = one.clone();
        changed.target = Target::parse("windows-x86").expect("valid triple");
        assert_ne!(changed.digest(), baseline, "target is bound");
        let mut changed = one.clone();
        changed.mode = HandoffMode::Upgrade;
        assert_ne!(changed.digest(), baseline, "mode is bound");
        let mut changed = one.clone();
        changed.session.cached_bytes = 0;
        assert_ne!(changed.digest(), baseline, "the session line is bound too");
    }

    #[test]
    fn a_traversing_variant_or_scope_is_refused() {
        let mut one = handoff();
        one.variant = "../../windows/system32".to_owned();
        assert!(one.validate().is_err());
        let mut two = handoff();
        two.scope = "machine/../user".to_owned();
        assert_eq!(two.validate(), Err(HandoffError::Scope));
    }
}
