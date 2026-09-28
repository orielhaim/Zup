//! The protocol version, and the one question a consumer asks about it.
//!
//! A wire contract that ships with its producer does not need a negotiation protocol,
//! a feature discovery handshake, or a SemVer parser. It needs a number in the
//! document and one comparison. Everything else — what a minor may add, what a major
//! may break, which unknown values a consumer skips — is a consequence of where that
//! comparison sits.
//!
//! ```text
//! accepts  same major, any minor      read it; ignore what you do not know
//! refuses  different major           do not guess
//! ```
//!
//! Minor increments are additive and never require a consumer to change: a new
//! optional field, a new event type, a new diagnostic code, a new artifact kind, a new
//! operation name. Major increments are semantic: a field changes meaning, changes
//! type, becomes required, or goes away.
//!
//! The two directions are asserted in this module's tests and again in
//! [`crate::tests::compatibility`], because a version check that is only exercised in
//! the accepting direction is a check that never rejects anything.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// The version this build of zup writes.
///
/// `1.0` is the first released contract. Nothing has ever been published under another
/// number, so there is no migration table and no legacy shape to keep alive.
pub const PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// A `MAJOR.MINOR` protocol version.
///
/// The generated TypeScript type is the string; the comparison is in the consumer's
/// decoder, where an unparseable version can be reported rather than defaulted.
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "bindings", ts(type = "string"))]
pub struct ProtocolVersion {
    /// Incremented by a semantic break. A consumer refuses a different value.
    pub major: u16,
    /// Incremented by an additive change. A consumer accepts any value.
    pub minor: u16,
}

/// Why a string is not a protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub text: String,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a protocol version; expected `MAJOR.MINOR`, for example `1.0`",
            self.text
        )
    }
}

impl std::error::Error for ProtocolError {}

impl ProtocolVersion {
    /// The version this build writes.
    pub const fn current() -> Self {
        PROTOCOL
    }

    /// Read a version, refusing anything that is not two decimal numbers.
    ///
    /// Bounded to `u16` on both halves so a version can be compared with `==` forever
    /// and a hostile document cannot smuggle in a value that overflows a consumer's
    /// own narrower type.
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let refuse = || ProtocolError {
            text: text.to_owned(),
        };
        let (major, minor) = text.split_once('.').ok_or_else(refuse)?;
        Ok(Self {
            major: major.parse().map_err(|_| refuse())?,
            minor: minor.parse().map_err(|_| refuse())?,
        })
    }

    /// Whether a consumer speaking `self` can read a producer speaking `other`.
    ///
    /// The whole policy, in one function. There is no third answer: a producer that
    /// names a newer major has changed something a consumer depends on, and guessing
    /// which thing it changed is how a release ships the wrong bytes.
    pub fn accepts(self, other: Self) -> bool {
        self.major == other.major
    }

    /// The same question with the two sides named, for a diagnostic that has to say
    /// which way round it went wrong.
    pub fn refuses(self, other: Self) -> bool {
        !self.accepts(other)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl std::str::FromStr for ProtocolVersion {
    type Err = ProtocolError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn version(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    /// The policy, in the accepting direction. A newer minor is the case the whole
    /// minor rule exists for, and it is the one a naive `==` check gets wrong.
    #[test]
    fn a_newer_minor_is_accepted() {
        assert!(version(1, 0).accepts(version(1, 0)));
        assert!(version(1, 0).accepts(version(1, 1)));
        assert!(version(1, 0).accepts(version(1, 99)));
        // Order does not matter: a consumer does not get to be newer than its producer
        // in a way that changes the answer.
        assert!(version(1, 4).accepts(version(1, 0)));
    }

    /// The policy, in the refusing direction. A different major is a refusal, not a
    /// best effort.
    #[test]
    fn a_different_major_is_refused_in_both_directions() {
        assert!(version(1, 0).refuses(version(2, 0)));
        assert!(version(2, 0).refuses(version(1, 0)));
        // The minor cannot rescue a major mismatch, in either direction.
        assert!(version(1, 9).refuses(version(2, 0)));
        assert!(version(2, 0).refuses(version(1, 9)));
    }

    #[test]
    fn the_wire_form_is_a_string_and_comes_back_the_same() {
        assert_eq!(PROTOCOL.to_string(), "1.0");
        let text = serde_json::to_string(&version(1, 3)).unwrap();
        assert_eq!(text, "\"1.3\"");
        assert_eq!(
            serde_json::from_str::<ProtocolVersion>(&text).unwrap(),
            version(1, 3)
        );
    }

    #[test]
    fn a_version_that_is_not_two_numbers_is_refused() {
        for text in [
            "", "1", "1.", ".0", "1.0.0", "one.zero", "1.0 ", "-1.0", "70000.0",
        ] {
            let error = serde_json::from_str::<ProtocolVersion>(&format!("\"{text}\""))
                .unwrap_err()
                .to_string();
            assert!(error.contains("not a protocol version"), "{text}: {error}");
        }
    }

    /// The version that ships is `1.0`, and the major is the number a consumer
    /// branches on. Asserted so a release cannot quietly move the contract.
    #[test]
    fn the_released_contract_is_one_zero() {
        assert_eq!(PROTOCOL, version(1, 0));
        assert!(PROTOCOL.accepts(ProtocolVersion::current()));
    }
}
