//! Signing evidence, and the transition from what a build produced to what will
//! be published.
//!
//! # Evidence is a list, not a flag
//!
//! A released file can carry several independent facts: a platform signature
//! structure, the platform's own judgement that the chain is trusted, a
//! publisher identity, a certificate identity, a timestamp. They fail separately
//! and mean different things, so a `signature: Signed { .. }` enum with one
//! shape for one platform would have to grow a variant per platform and a field
//! per fact, and the fields a given platform cannot produce would sit in the
//! document as `false`.
//!
//! So evidence is a list of `(fact, value)` pairs and the value is whatever the
//! platform that established the fact words it. A consumer that needs a specific
//! fact asks for it by name; a consumer that needs "is there a signature over
//! these bytes" asks [`covers_bytes`]. Nothing here pretends every platform can
//! answer every question.
//!
//! # What is not evidence
//!
//! A build attestation, Sigstore build provenance, TUF metadata and a
//! content-addressed digest are all supply-chain facts about a file, and none of
//! them is a signature *in* the file. They are recorded elsewhere, checked
//! elsewhere, and deliberately kept out of this type: a release that carries an
//! attestation record and no platform signature must be describable without
//! pretending the attestation was one.

use std::path::Path;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::plan::SigningSubject;

/// What kind of fact one piece of evidence is.
///
/// A closed set, because a fact nobody can name cannot be asked for. A platform
/// with a fact this list does not have is a platform that needs an entry here,
/// which is a deliberate decision rather than a free-text field that quietly
/// absorbs anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFact {
    /// A platform signature structure covers exactly these bytes.
    ///
    /// The strongest statement that can be made from the file alone: the
    /// embedded digest matches the bytes the release publishes. It says nothing
    /// about whether the signing chain is *trusted* — a self-signed development
    /// certificate produces this and nothing more — which is what
    /// [`EvidenceFact::PlatformTrustAccepted`] is for.
    SignatureCoversBytes,
    /// The platform's own trust policy accepted the signing chain.
    ///
    /// Only a platform can answer this: it is a statement about the verifying
    /// machine's certificate store, not about the file. A non-Windows verifier
    /// can compute [`EvidenceFact::SignatureCoversBytes`] and must not record
    /// this one.
    PlatformTrustAccepted,
    /// The publisher identity read out of the signature.
    Publisher,
    /// The signing certificate's own identity.
    Certificate,
    /// The timestamp the signature carries, and which kind.
    Timestamp,
}

impl EvidenceFact {
    /// The wire name, for a report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignatureCoversBytes => "signature_covers_bytes",
            Self::PlatformTrustAccepted => "platform_trust_accepted",
            Self::Publisher => "publisher",
            Self::Certificate => "certificate",
            Self::Timestamp => "timestamp",
        }
    }
}

/// One fact a platform established about a signed subject.
///
/// `value` is the platform's own wording of it: a certificate subject in its
/// display form, a thumbprint as it prints it, a timestamp kind. A consumer
/// switches on [`SigningEvidence::fact`] before reading it, so the string is not
/// a format any portable code has to agree on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningEvidence {
    pub fact: EvidenceFact,
    pub value: String,
}

impl SigningEvidence {
    pub fn new(fact: EvidenceFact, value: impl Into<String>) -> Self {
        Self {
            fact,
            value: value.into(),
        }
    }
}

/// Whether this evidence states that a platform signature covers these bytes.
///
/// The one question a release description has to be able to answer, and the one
/// that cannot be answered by the absence of a flag: an unsigned release is
/// *finalized* — it has a real published identity, because nothing changed the
/// bytes — and it is not signed, and no consumer of the document may conclude
/// otherwise.
pub fn covers_bytes(evidence: &[SigningEvidence]) -> bool {
    value_of(evidence, EvidenceFact::SignatureCoversBytes).is_some()
}

/// The publisher identity, if the platform could read one.
pub fn publisher(evidence: &[SigningEvidence]) -> Option<&str> {
    value_of(evidence, EvidenceFact::Publisher)
}

/// The certificate identity, if the platform could read one.
pub fn thumbprint(evidence: &[SigningEvidence]) -> Option<&str> {
    value_of(evidence, EvidenceFact::Certificate)
}

/// The timestamp, if the signature carries one.
pub fn timestamp(evidence: &[SigningEvidence]) -> Option<&str> {
    value_of(evidence, EvidenceFact::Timestamp)
}

fn value_of(evidence: &[SigningEvidence], fact: EvidenceFact) -> Option<&str> {
    evidence
        .iter()
        .find(|entry| entry.fact == fact)
        .map(|entry| entry.value.as_str())
}

/// A measurement of some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub digest: Sha256Digest,
    pub size: u64,
}

impl Measured {
    /// Measure a file, streaming it.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let (size, digest) = zup_core::hash_reader(std::io::BufReader::new(file))?;
        Ok(Self { digest, size })
    }
}

/// The identity a subject is published under, and the evidence about it.
///
/// The fields are private because the value is only obtainable one way:
/// [`finalize`], which measures the file. A published identity is a measurement,
/// not a transcription of what a signing tool said it did, and a type whose
/// fields were public could be built by copying someone else's digest — which is
/// the failure the whole finalization step exists to prevent.
///
/// A *parsed* release description is the one exception, and it is a different
/// thing: a publisher reading a manifest somebody else wrote is reading a claim,
/// and re-measures the bytes before it uploads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizedArtifact {
    digest: Sha256Digest,
    size: u64,
    evidence: Vec<SigningEvidence>,
}

impl FinalizedArtifact {
    /// The digest of the exact bytes that will be published.
    pub fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// The size of those bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// What the platform established about the signature.
    pub fn evidence(&self) -> &[SigningEvidence] {
        &self.evidence
    }

    /// Whether these bytes carry a platform signature.
    pub fn is_signed(&self) -> bool {
        covers_bytes(&self.evidence)
    }
}

/// Measure a subject and produce the identity it will be published under.
///
/// The obligation this encodes is the order: **every byte-mutating signing step
/// has already happened.** `claimed` is what the caller measured immediately
/// after verifying the signature, and this function measures again. They must
/// agree, or the file changed between the two and the identity either one
/// describes is not the identity of the bytes on disk — which is the one thing a
/// published release must never be.
///
/// Passing an *unsigned* release through here is legitimate and is how an
/// internal distribution is finalized: `evidence` is empty, the measurement is
/// the same measurement, and [`FinalizedArtifact::is_signed`] reports false.
pub fn finalize(
    subject: &SigningSubject,
    release_root: &Path,
    claimed: &Measured,
    evidence: Vec<SigningEvidence>,
) -> Result<FinalizedArtifact, FinalizationError> {
    let path = subject.resolve(release_root);
    let measured = Measured::of(&path).map_err(|error| FinalizationError::Unreadable {
        subject: subject.path.clone(),
        reason: error.to_string(),
    })?;
    if measured != *claimed {
        return Err(FinalizationError::BytesChanged {
            subject: subject.path.clone(),
            digest: measured.digest,
            size: measured.size,
            claimed: claimed.digest,
            claimed_size: claimed.size,
        });
    }
    Ok(FinalizedArtifact {
        digest: measured.digest,
        size: measured.size,
        evidence,
    })
}

/// Why a subject could not be finalized.
#[derive(Debug, thiserror::Error)]
pub enum FinalizationError {
    #[error(
        "`{subject}` is {size} bytes with sha256:{digest}, not the {claimed_size} bytes with sha256:{claimed} that were just measured; the bytes changed after they were signed"
    )]
    BytesChanged {
        subject: String,
        digest: Sha256Digest,
        size: u64,
        claimed: Sha256Digest,
        claimed_size: u64,
    },
    #[error("`{subject}` could not be measured: {reason}")]
    Unreadable { subject: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> SigningSubject {
        SigningSubject {
            path: "dist/Acme-Setup.exe".to_owned(),
            digest: Sha256Digest::from_bytes([0x11; 32]),
            size: 8,
            variants: Vec::new(),
        }
    }

    fn write(contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::TempDir::new().expect("temp dir");
        // The subject is `dist/Acme-Setup.exe`, so the release root the tests
        // measure against has a `dist/` in it.
        let dist = directory.path().join("dist");
        std::fs::create_dir(&dist).expect("create dist");
        let path = dist.join("Acme-Setup.exe");
        std::fs::write(&path, contents).expect("write");
        (directory, path)
    }

    fn signed_evidence() -> Vec<SigningEvidence> {
        vec![
            SigningEvidence::new(EvidenceFact::SignatureCoversBytes, "sha256"),
            SigningEvidence::new(EvidenceFact::PlatformTrustAccepted, "wintrust"),
            SigningEvidence::new(EvidenceFact::Publisher, "CN=Acme"),
            SigningEvidence::new(EvidenceFact::Certificate, "0011AABB"),
            SigningEvidence::new(EvidenceFact::Timestamp, "rfc3161"),
        ]
    }

    /// A release description records several independent facts about a
    /// signature, and they are individually addressable. A boolean could not say
    /// which of them held.
    #[test]
    fn evidence_carries_several_independently_addressable_facts() {
        let evidence = signed_evidence();
        assert!(covers_bytes(&evidence));
        assert_eq!(publisher(&evidence), Some("CN=Acme"));
        assert_eq!(thumbprint(&evidence), Some("0011AABB"));
        assert_eq!(timestamp(&evidence), Some("rfc3161"));
        assert_eq!(
            evidence.len(),
            5,
            "a fact recorded twice would be read once, silently"
        );
    }

    /// The distinction the whole type exists for: an unsigned release is
    /// finalized — it has a real published identity, because nothing changed the
    /// bytes — and it is not signed.
    #[test]
    fn an_unsigned_release_is_finalized_and_reports_that_it_is_not_signed() {
        let (directory, path) = write(b"unsigned!");
        let claimed = Measured::of(&path).expect("measure");
        let finalized = finalize(&subject(), directory.path(), &claimed, Vec::new())
            .expect("an unsigned release finalizes");
        assert!(!finalized.is_signed());
        assert_eq!(finalized.size(), 9);
        assert_eq!(finalized.digest(), &claimed.digest);
    }

    /// A signature appends a certificate table, so the published identity and
    /// the built identity differ; and a file that changed between the signature
    /// check and the finalization is a release whose published digest names
    /// bytes that are not on disk.
    #[test]
    fn bytes_that_change_between_verification_and_finalization_are_refused() {
        let (directory, _path) = write(b"unsigned plus a certificate table");
        let claimed = Measured {
            digest: Sha256Digest::from_bytes([0x22; 32]),
            size: 4096,
        };
        let error = finalize(&subject(), directory.path(), &claimed, signed_evidence())
            .expect_err("a changed file is refused");
        assert!(matches!(error, FinalizationError::BytesChanged { .. }));
    }

    /// The measurement is the obligation. There is no constructor that takes a
    /// digest, so a caller cannot publish an identity it was told about instead
    /// of one it measured.
    #[test]
    fn the_published_identity_is_measured_from_the_file() {
        let (directory, path) = write(b"signed bytes");
        let claimed = Measured::of(&path).expect("measure");
        let finalized =
            finalize(&subject(), directory.path(), &claimed, signed_evidence()).expect("finalize");
        assert!(finalized.is_signed());
        assert_eq!(finalized.digest(), &zup_core::hash_bytes(b"signed bytes"));

        let error = finalize(
            &SigningSubject {
                path: "dist/nope.exe".to_owned(),
                ..subject()
            },
            directory.path(),
            &claimed,
            signed_evidence(),
        )
        .expect_err("a subject that is not there cannot be finalized");
        assert!(matches!(error, FinalizationError::Unreadable { .. }));
    }

    /// Evidence is written into a published document. A field that could hold a
    /// secret would leak into every release description.
    #[test]
    fn evidence_holds_no_credential() {
        let encoded = serde_json::to_string(&signed_evidence()).expect("encode");
        for word in [
            "password",
            "pfx",
            "secret",
            "token",
            "private",
            "credential",
        ] {
            assert!(!encoded.contains(word), "the evidence mentions `{word}`");
        }
    }
}
