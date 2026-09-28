//! The handoff: what one process tells the next about what it already did.
//!
//! A thin bootstrapper resolves a release, verifies a native runtime, and starts
//! it. Two properties have to hold at that moment:
//!
//! 1. **The native runtime must not take the bootstrapper's word for anything
//!    that matters.** It is about to install files, registry entries, and
//!    services on a user's machine. A bootstrapper that can name a different
//!    release, a different variant, a different target, or a different payload
//!    is a remote code execution primitive.
//! 2. **The user must not experience it as two installers.** The progress they
//!    were watching has to continue rather than restart at zero.
//!
//! Both are solved by the same object. Every field in [`RuntimeHandoff`] is a
//! digest, a name, or a count, and the whole document has its own digest. The
//! binding that makes it more than a hint is one line in the runtime's own
//! verification:
//!
//! ```text
//! handoff.runtime == sha256(this executable)
//! ```
//!
//! A bootstrapper cannot substitute a release, because the release has to name
//! *this* image's digest as one of its variants, and a release that does is a
//! release whose fingerprint the client has already checked. It cannot
//! substitute a payload, because every payload blob is named by digest in the
//! authenticated catalog. It cannot substitute a target, because the runtime
//! compares the handoff's target against the target compiled into its own
//! embedded plan.
//!
//! What the bootstrapper *does* supply is a location: where the verified cache
//! is, and where this document is. A hostile location yields a hostile cache
//! full of blobs that do not hash to the authenticated digests, which is a
//! failure, not a compromise. Locations are therefore the only untrusted part,
//! and they are labelled as such wherever they are passed.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::{AppId, Sha256Digest, TargetTriple};

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
    /// A first install from an authenticated release.
    Install,
    /// A move to a newer release.
    Upgrade,
    /// A different component selection of the same release.
    Modify,
    /// Restoring committed ownership after drift.
    Repair,
}

impl HandoffMode {
    /// The lifecycle verb a command line uses.
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Modify => "modify",
            Self::Repair => "repair",
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
    pub schema: u32,
    pub app_id: AppId,
    /// The release graph, identified by its own recomputed fingerprint.
    pub release: Sha256Digest,
    /// The digest of the release document's *bytes*, which is how a content cache
    /// is addressed.
    ///
    /// This is the one field a bootstrapper supplies freely, and it is a cache
    /// key rather than an authority: pointing it at a different document
    /// selects bytes whose recomputed `release` fingerprint is not the `release`
    /// above, and the check fails. Two digests are needed because a release
    /// document names its own fingerprint as a field — it cannot also be the hash
    /// of its own encoding — while a cache can only be keyed by a hash.
    pub document: Sha256Digest,
    /// The content catalog that release names.
    pub catalog: Sha256Digest,
    /// The variant chosen for this host.
    pub variant: String,
    /// The variant descriptor: the install plan the runtime is about to act on.
    pub manifest: Sha256Digest,
    /// The native runtime image.
    ///
    /// The receiving process checks this against the hash of its own file. That
    /// single check is what binds every other field, because the release names
    /// this digest and the release's fingerprint was verified before anything
    /// was downloaded.
    pub runtime: Sha256Digest,
    pub target: TargetTriple,
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
        crate::layout::check_segment(&self.variant).map_err(HandoffError::Variant)?;
        if !matches!(self.scope.as_str(), "user" | "machine") {
            return Err(HandoffError::Scope);
        }
        for component in &self.components {
            crate::layout::check_segment(component).map_err(HandoffError::Variant)?;
        }
        Ok(())
    }

    /// This handoff's own fingerprint.
    ///
    /// A file whose bytes hash to this is the handoff the process was meant to
    /// be handed. It is not a signature — nothing here needs one, because the
    /// document is not the authority; `runtime` is.
    pub fn digest(&self) -> Sha256Digest {
        let body = HandoffBody {
            schema: self.schema,
            app_id: &self.app_id,
            release: self.release,
            document: self.document,
            catalog: self.catalog,
            variant: &self.variant,
            manifest: self.manifest,
            runtime: self.runtime,
            target: &self.target,
            frontend: &self.frontend,
            mode: self.mode,
            scope: &self.scope,
            session: &self.session,
            components: &self.components,
        };
        let bytes = serde_json::to_vec(&body).expect("a handoff body always serializes");
        Sha256Digest::from_bytes(Sha256::digest(&bytes).into())
    }

    /// The identity an installation persists when this handoff commits.
    ///
    /// `trust_anchor` is supplied by the caller because it is the digest of the
    /// root the release was authenticated under, which the acquisition engine
    /// deliberately does not hold.
    pub fn identity(
        &self,
        version: &str,
        channel: &str,
        pinned: bool,
        trust_anchor: Sha256Digest,
    ) -> zup_core::ReleaseIdentity {
        zup_core::ReleaseIdentity {
            app_id: self.app_id.clone(),
            release: self.release,
            catalog: self.catalog,
            variant: self.variant.clone(),
            manifest: self.manifest,
            runtime: Some(self.runtime),
            version: version.to_owned(),
            target: self.target.clone(),
            channel: channel.to_owned(),
            pinned,
            trust_anchor,
            frontend: self.frontend.clone(),
            components: self.components.clone(),
        }
    }
}

/// The part of a handoff its fingerprint covers. Every field is, including the
/// session summary: a progress line that disagrees with the document it came
/// from is a bug worth catching, and it costs nothing to include.
#[derive(Debug, Serialize)]
struct HandoffBody<'a> {
    schema: u32,
    app_id: &'a AppId,
    release: Sha256Digest,
    document: Sha256Digest,
    catalog: Sha256Digest,
    variant: &'a str,
    manifest: Sha256Digest,
    runtime: Sha256Digest,
    target: &'a TargetTriple,
    frontend: &'a str,
    mode: HandoffMode,
    scope: &'a str,
    session: &'a SessionSummary,
    components: &'a [String],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff() -> RuntimeHandoff {
        RuntimeHandoff {
            schema: HANDOFF_SCHEMA,
            app_id: AppId::new("com.example.app").expect("valid app id"),
            release: Sha256Digest::from_bytes([1; 32]),
            document: Sha256Digest::from_bytes([5; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "x86_64-pc-windows-msvc".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: Sha256Digest::from_bytes([4; 32]),
            target: TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid triple"),
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
        changed.variant = "aarch64-pc-windows-msvc".to_owned();
        assert_ne!(changed.digest(), baseline, "variant is bound");
        let mut changed = one.clone();
        changed.target = TargetTriple::parse("i686-pc-windows-msvc").expect("valid triple");
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
