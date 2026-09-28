//! What a build produced, in one shape.
//!
//! One DTO for every file a zup operation produces: an installer, a thin
//! bootstrapper, a transport package, a staged document. Before this existed, a build
//! printed a paragraph per artifact, `zup artifact inspect` printed a different
//! document, and the Action hand-wrote a third that it filled in from a release
//! manifest. Three shapes for one concept is three places a new field has to be
//! remembered, and the memory is what fails.
//!
//! Two decisions are load-bearing:
//!
//! - **The path is release-relative.** A build-machine absolute path is not an
//!   identity: the same artifact built on a different agent lives somewhere else, and a
//!   consumer that recorded the absolute path recorded a fact about the machine rather
//!   than about the release. `zup build` already refuses a release whose outputs do not
//!   share one directory, precisely because a release is one root.
//! - **The kind and mode are open identifiers.** `single`, `universal`, `offline`,
//!   `thin` are today's values; `bound` or `delta` can join them without a major bump, and
//!   a consumer that meets one it does not know displays it rather than failing. The same
//!   reason means an internal Rust enum is never serialized directly: `Debug` output is a
//!   rendering of a Rust type, and a renamed variant would silently change a wire
//!   contract.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::identifier::Identifier;

/// The largest integer every JSON consumer that uses a double can hold exactly.
///
/// Chosen rather than derived: `2^53` is the point where incrementing by one stops
/// being observable in a JavaScript `number`, and every consumer of this contract that
/// is not Rust is that.
pub const MAX_SAFE_BYTES: u64 = (1 << 53) - 1;

/// A byte count that is safe in every language that reads the contract.
///
/// A `u64` on the wire is a silent bug waiting for a large file. Producers saturate
/// rather than fail, because a byte count that is one past the limit is still a
/// monotonically increasing answer and a producer that refused to report a 9 petabyte
/// artifact would be refusing to do its job; consumers, and anything that reads a
/// document from somewhere else, get a hard error so an unrepresentable value can
/// never be laundered into a wrong one.
///
/// The generated TypeScript type is a plain `number`, not a `bigint`. `ts-rs` maps
/// `u64` to `bigint` on its own, and that would be a lie: the wire format carries a
/// JSON number, and a consumer that read one into a `bigint` would have to serialize it
/// back as a string to produce a document this protocol accepts. The bound is what
/// makes `number` correct, and the `JsonSchema` implementation in [`crate::schema`]
/// says so where a validator can see it.
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "bindings", ts(type = "number"))]
pub struct ByteCount(u64);

impl ByteCount {
    /// The largest byte count a consumer can represent exactly.
    pub const MAX: ByteCount = ByteCount(MAX_SAFE_BYTES);

    /// A byte count known to be representable.
    pub fn new(bytes: u64) -> Self {
        if bytes > MAX_SAFE_BYTES {
            return Self(MAX_SAFE_BYTES);
        }
        Self(bytes)
    }

    /// A byte count, or a refusal when it cannot be represented.
    pub const fn checked(bytes: u64) -> Option<Self> {
        if bytes <= MAX_SAFE_BYTES {
            Some(Self(bytes))
        } else {
            None
        }
    }

    /// The count as a `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether the value would survive a round trip through a double.
    pub const fn is_exact(self) -> bool {
        self.0 <= MAX_SAFE_BYTES
    }
}

impl From<u64> for ByteCount {
    fn from(bytes: u64) -> Self {
        Self::new(bytes)
    }
}

impl Serialize for ByteCount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = u64::deserialize(deserializer)?;
        Self::checked(bytes).ok_or_else(|| {
            D::Error::custom(format!(
                "{bytes} is larger than {MAX_SAFE_BYTES} and cannot be represented exactly by \
                 every consumer of this protocol"
            ))
        })
    }
}

/// The algorithm behind a digest, and the value it produced.
///
/// Two fields rather than a `sha256:<hex>` string, because a consumer has to be able
/// to compare the algorithm to decide whether a comparison means anything. A prefixed
/// string is one field a consumer has to split and get right for every value.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// The digest algorithm, lowercased. `sha256` is the only one zup emits today.
    pub algorithm: String,
    /// The digest, lowercase hex.
    pub value: String,
}

impl Digest {
    /// A SHA-256 digest from its hex form.
    pub fn sha256(value: impl Into<String>) -> Self {
        Self {
            algorithm: "sha256".to_owned(),
            value: value.into().to_ascii_lowercase(),
        }
    }

    /// Whether a lowercase hex string of the right length for the algorithm.
    ///
    /// Checked at the boundary rather than trusted, because a digest that is the wrong
    /// length is a digest nothing downstream can use and a consumer that passed it on
    /// would fail far from the cause.
    pub fn is_well_formed(&self) -> bool {
        let expected = match self.algorithm.as_str() {
            "sha256" => 64,
            "sha512" => 128,
            _ => return false,
        };
        self.value.len() == expected
            && self
                .value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

/// What was established about a file's signature.
///
/// A list rather than a state word, because the facts fail separately and mean
/// different things: a signature can cover the bytes without the platform trusting the
/// chain, and either can hold without a publisher. A single `signed`/`unsigned` field
/// cannot say which, so it would either overstate or say nothing.
///
/// `fact` is open on purpose. Today's vocabulary is `signature_covers_bytes`,
/// `platform_trust_accepted`, `publisher`, `certificate` and `timestamp`; a new fact is
/// an additive change, and a consumer that displays an unknown one is doing the right
/// thing. `value` is the platform's own wording, because translating it would be zup
/// making a claim the platform did not.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningEvidence {
    pub fact: String,
    pub value: String,
}

impl SigningEvidence {
    /// One established fact.
    pub fn new(fact: &str, value: impl Into<String>) -> Self {
        Self {
            fact: fact.to_owned(),
            value: value.into(),
        }
    }

    /// Whether the platform established that a signature covers these bytes.
    pub fn covers_bytes(evidence: &[SigningEvidence]) -> bool {
        evidence
            .iter()
            .any(|entry| entry.fact == "signature_covers_bytes")
    }
}

/// A file a zup operation produced, or a provider published.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Release-relative, `/`-separated. Never a build-machine absolute path.
    pub path: String,
    /// The digest of the bytes as they stand now.
    pub digest: Digest,
    /// The size of those bytes.
    pub size: ByteCount,
    /// What the artifact is: `single`, `universal`, `thin`, `package`, `document`.
    pub kind: Identifier,
    /// How it behaves: `offline`, `thin`, `package`.
    pub mode: Identifier,
    /// The declared artifact id, when the manifest named one.
    pub id: Option<String>,
    /// The target triple, for an artifact that serves exactly one machine.
    pub target: Option<String>,
    /// The variant ids this artifact carries, for one that serves several.
    pub variants: Option<Vec<String>>,
    /// What is known about its signature, once signing has been attempted.
    ///
    /// `null` means nobody has looked, which is a different fact from an empty list
    /// meaning "looked, and there is no signature". An unsigned release is a real
    /// release: it is finalized, it has a published identity, and nothing proves who
    /// produced it.
    pub signing: Option<SigningState>,
}

/// How far signing got for one artifact.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningState {
    /// `unsigned`, `signed`, or `unverified` when a signature exists but has not been
    /// accepted against a policy.
    pub state: Identifier,
    /// The facts the platform established, in the order it established them.
    pub evidence: Vec<SigningEvidence>,
}

impl SigningState {
    /// A state with no evidence, for a file nobody has signed.
    pub fn unsigned() -> Self {
        Self {
            state: Identifier::fixed("unsigned"),
            evidence: Vec::new(),
        }
    }

    /// A state built from accepted evidence.
    pub fn signed(evidence: Vec<SigningEvidence>) -> Self {
        Self {
            state: Identifier::fixed("signed"),
            evidence,
        }
    }

    /// Whether a signature covers these bytes.
    pub fn covers_bytes(&self) -> bool {
        SigningEvidence::covers_bytes(&self.evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_count_saturates_on_the_way_out_and_refuses_on_the_way_in() {
        assert_eq!(ByteCount::new(u64::MAX).get(), MAX_SAFE_BYTES);
        assert_eq!(ByteCount::checked(u64::MAX), None);
        assert_eq!(
            ByteCount::checked(MAX_SAFE_BYTES).map(ByteCount::get),
            Some(MAX_SAFE_BYTES)
        );
        assert!(
            ByteCount::checked(1 << 53).is_none(),
            "2^53 is the first value a double cannot increment"
        );
        // Serializing is a plain number, so a JavaScript consumer reads it as one.
        assert_eq!(serde_json::to_string(&ByteCount::new(42)).unwrap(), "42");
    }

    /// The producer's side and the consumer's side are the same invariant, and the
    /// consumer's side is the one that matters: a document carrying a value no consumer
    /// can hold is a document whose size field is already wrong by the time anybody
    /// reads it.
    #[test]
    fn a_byte_count_from_the_wire_outside_the_safe_range_is_refused() {
        let over = format!("{}", MAX_SAFE_BYTES + 1);
        let error = serde_json::from_str::<ByteCount>(&over)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be represented exactly"), "{error}");
        let at = format!("{MAX_SAFE_BYTES}");
        assert_eq!(
            serde_json::from_str::<ByteCount>(&at).unwrap().get(),
            MAX_SAFE_BYTES
        );
    }

    #[test]
    fn a_digest_states_its_algorithm_and_is_checked_at_the_boundary() {
        let digest = Digest::sha256("A".repeat(64));
        assert_eq!(digest.algorithm, "sha256");
        assert!(digest.is_well_formed());
        assert!(!Digest::sha256("a".repeat(63)).is_well_formed());
        assert!(!Digest::sha256("z".repeat(64)).is_well_formed());
        assert!(
            !Digest {
                algorithm: "crc32".to_owned(),
                value: "a".repeat(64)
            }
            .is_well_formed()
        );
    }

    /// The distinction the whole `signing` field exists to keep: nobody has looked, or
    /// nobody found anything.
    #[test]
    fn an_unsigned_artifact_and_an_unexamined_one_are_different_facts() {
        let unsigned = SigningState::unsigned();
        assert!(!unsigned.covers_bytes());
        let signed = SigningState::signed(vec![SigningEvidence::new(
            "signature_covers_bytes",
            "sha256",
        )]);
        assert!(signed.covers_bytes());
        let artifact = Artifact {
            path: "Acme-Setup.exe".to_owned(),
            digest: Digest::sha256("a".repeat(64)),
            size: ByteCount::new(1024),
            kind: Identifier::fixed("single"),
            mode: Identifier::fixed("offline"),
            id: Some("windows-x64".to_owned()),
            target: Some("x86_64-pc-windows-msvc".to_owned()),
            variants: None,
            signing: None,
        };
        let text = serde_json::to_string(&artifact).unwrap();
        assert!(text.contains("\"signing\":null"), "{text}");
        assert!(
            !text.contains("debug") && !text.contains("Debug"),
            "an artifact is not a Debug rendering: {text}"
        );
    }
}
