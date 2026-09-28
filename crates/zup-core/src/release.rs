//! The exact release an installation was produced from.
//!
//! A version number is not identity. Two builds can carry the same `1.4.0` and
//! install different bytes on the same machine - a different target, a
//! different component selection, a rebuild that changed a single resource. An
//! upgrade, a repair, or a recovery that trusts the version alone will happily
//! act on the wrong graph, so every claim that has to survive a restart is a
//! digest.
//!
//! This is the record an installation persists, and it is the same record the
//! three later operations read:
//!
//! - an **update** resolves a newer release and compares digests;
//! - a **repair** names the variant and components whose content it must
//!   reacquire;
//! - a **recovery** re-derives the transaction from the release it committed.
//!
//! It lives in `zup-core` rather than beside the acquisition engine because two
//! crates need it and neither may depend on the other: the engine that produces
//! it and the ownership ledger that stores it.

use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;
use crate::ids::AppId;
use crate::target::TargetTriple;

/// Bound on the number of components one identity may record.
pub const MAX_IDENTITY_COMPONENTS: usize = 1024;

/// What an installed machine persisted about the release that produced it.
///
/// `version` and `channel` are recorded for humans and for a window title.
/// Everything that a later operation acts on is a digest, and the two are never
/// interchangeable: a machine that only knows `1.4.0` cannot tell a correct
/// upgrade from one that would install a different machine's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub app_id: AppId,
    /// Identity of the exact release graph, recomputed from its own body.
    pub release: Sha256Digest,
    /// The content catalog that release names.
    pub catalog: Sha256Digest,
    /// The variant that was installed.
    pub variant: String,
    /// The variant descriptor: the install plan this machine acted on.
    pub manifest: Sha256Digest,
    /// The native runtime image the installation maintains, when the release
    /// carries one. An offline release embeds it in the installer instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Sha256Digest>,
    /// The version that graph claimed.
    pub version: String,
    pub target: TargetTriple,
    /// The channel the release was resolved through, or empty when the release
    /// was addressed by version.
    #[serde(default)]
    pub channel: String,
    /// Whether this identity came from a version pin rather than a channel.
    ///
    /// A pinned installer must not silently become a later release, and this is
    /// the flag that keeps a later operation from treating it as one.
    #[serde(default)]
    pub pinned: bool,
    /// Identity of the publisher's trust anchor: the digest of the TUF root the
    /// release was authenticated under.
    ///
    /// Two repositories can serve two graphs with the same application id and
    /// the same version. This is what tells them apart.
    pub trust_anchor: Sha256Digest,
    /// `gui` or `console`, which is what the installation presents as.
    #[serde(default)]
    pub frontend: String,
    /// The components that were installed, which together with the variant
    /// digest decide exactly what a repair has to restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
}

/// Why an identity could not describe an installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("a release identity names no version")]
    Version,
    #[error("a release identity names no variant")]
    Variant,
    #[error("a release identity names more components than this build accepts")]
    TooManyComponents,
    #[error("a release identity names an invalid segment: {0}")]
    Segment(&'static str),
    #[error("a release identity names an invalid channel")]
    Channel,
}

/// The character set every single-segment name in an identity must use.
///
/// This is deliberately the same rule the content layout applies to a variant
/// name and a channel name: an identity segment reaches a directory name and a
/// URL, so it is bounded to characters that cannot escape either.
fn check_segment(name: &str) -> Result<(), IdentityError> {
    if name.is_empty() {
        return Err(IdentityError::Variant);
    }
    if name.len() > 64 {
        return Err(IdentityError::Segment("a segment cannot exceed 64 bytes"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.')
    {
        return Err(IdentityError::Segment(
            "a segment must be ASCII letters, digits, `-`, `_`, or `.`",
        ));
    }
    if name.starts_with('.') {
        return Err(IdentityError::Segment("a segment cannot start with `.`"));
    }
    Ok(())
}

impl ReleaseIdentity {
    /// Whether this identity came from a version pin.
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether this identity describes the same release graph as `other`.
    ///
    /// This is the comparison an update makes before it fetches anything: two
    /// identities with the same version and different digests are two different
    /// installations' worth of claim, and the release digest is the one that
    /// decides.
    pub fn same_release(&self, other: &Self) -> bool {
        self.release == other.release && self.catalog == other.catalog
    }

    /// Reject an identity that could not describe an installation.
    pub fn validate(&self) -> Result<(), IdentityError> {
        if self.version.is_empty() {
            return Err(IdentityError::Version);
        }
        check_segment(&self.variant)?;
        if !self.channel.is_empty() {
            check_channel(&self.channel)?;
        }
        if self.components.len() > MAX_IDENTITY_COMPONENTS {
            return Err(IdentityError::TooManyComponents);
        }
        for component in &self.components {
            check_segment(component).map_err(|_| IdentityError::Segment("invalid component"))?;
        }
        Ok(())
    }

    /// Encode an identity for the ownership ledger.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        self.validate()
            .map_err(|error| serde::ser::Error::custom(error.to_string()))?;
        serde_json::to_vec(self)
    }
}

fn check_channel(channel: &str) -> Result<(), IdentityError> {
    if channel.is_empty() || channel.len() > 32 {
        return Err(IdentityError::Channel);
    }
    if !channel
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(IdentityError::Channel);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ReleaseIdentity {
        ReleaseIdentity {
            app_id: AppId::new("com.example.app").expect("valid app id"),
            release: Sha256Digest::from_bytes([1; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "x86_64-pc-windows-msvc".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: Some(Sha256Digest::from_bytes([4; 32])),
            version: "1.4.0".to_owned(),
            target: TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid target triple"),
            channel: "stable".to_owned(),
            pinned: false,
            trust_anchor: Sha256Digest::from_bytes([5; 32]),
            frontend: "gui".to_owned(),
            components: vec!["core".to_owned()],
        }
    }

    #[test]
    fn a_traversing_segment_is_refused() {
        let mut one = identity();
        one.variant = "../escape".to_owned();
        assert!(one.validate().is_err());
        let mut two = identity();
        two.channel = "Stable".to_owned();
        assert_eq!(two.validate(), Err(IdentityError::Channel));
        assert!(one.encode().is_err());
    }
}
