//! One validated, open-ended identifier, and the errors a bad one produces.
//!
//! Operation names, artifact kinds, artifact modes, publication states and diagnostic
//! codes are all the same shape of thing: a lowercase, dotted, forward-only vocabulary
//! that zup extends and never renames. Enumerating them in Rust would make every
//! extension a wire break, which is exactly backwards — the vocabulary is supposed to
//! grow, and a consumer that meets a value it does not know is required to keep going.
//!
//! What *is* enforced is the grammar, because an unvalidated string on a wire is an
//! invitation to a consumer that has to handle whitespace, casing and separators it
//! never agreed to:
//!
//! ```text
//! segment  ::= [a-z] ( [a-z0-9] | separator word )*
//! name     ::= segment ('.' segment)*
//! ```
//!
//! A segment starts with a lowercase letter, ends with a letter or a digit, and joins
//! words with `_` or `-` but never with two separators in a row. So `build`,
//! `publish.github`, `zup.manifest.unknown_target` and `single-target` are
//! identifiers; `Build`, `publish..github`, `zup.manifest.`, `zup/manifest` and
//! `unknown__target` are not. The rule is applied on the way in and on the way out, so
//! a document that came from somewhere else cannot smuggle an unbounded string into a
//! consumer's matching.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// A wire identifier that follows the grammar above.
///
/// The generated TypeScript type is the string it parses from. A branded string would
/// be more precise and less useful: a consumer holding a document has to turn it into
/// this type somehow, and the brand would only tell it that whatever it built is the
/// brand. The validation lives in the consumer's decoder, where an ungrammatical value
/// can still be reported.
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "bindings", ts(type = "string"))]
pub struct Identifier(String);

/// Why a string is not a wire identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentifierError {
    /// The value was empty.
    Empty,
    /// The value did not match the grammar.
    Invalid { value: String },
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "an identifier cannot be empty"),
            Self::Invalid { value } => write!(
                f,
                "`{value}` is not an identifier: expected lowercase dotted segments such as \
                 `build` or `zup.manifest.unknown_target`"
            ),
        }
    }
}

impl std::error::Error for IdentifierError {}

impl Identifier {
    /// An identifier known at compile time.
    ///
    /// Not `parse`: the constants this crate and its callers use are literals that
    /// satisfy the grammar by construction, and routing them through a fallible
    /// constructor would mean an `expect` at every call site for a mistake the
    /// compiler already rules out.
    pub fn fixed(name: &'static str) -> Self {
        Self(name.to_owned())
    }

    /// Check a string against the grammar.
    pub fn parse(value: &str) -> Result<Self, IdentifierError> {
        if value.is_empty() {
            return Err(IdentifierError::Empty);
        }
        if is_identifier(value) {
            return Ok(Self(value.to_owned()));
        }
        Err(IdentifierError::Invalid {
            value: value.to_owned(),
        })
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the identifier a caller named.
    pub fn is(&self, other: &str) -> bool {
        self.0 == other
    }

    /// Take the identifier apart into its dotted segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Identifier> for String {
    fn from(value: Identifier) -> Self {
        value.0
    }
}

impl Serialize for Identifier {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Identifier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

impl FromStr for Identifier {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// The grammar, as one predicate.
///
/// A hand-written walk rather than a regular expression: this crate's whole dependency
/// budget is `serde`, and a regex engine to validate a twenty-character word is not a
/// trade worth making.
fn is_identifier(value: &str) -> bool {
    value.split('.').all(is_segment) && !value.is_empty()
}

fn is_segment(segment: &str) -> bool {
    let mut characters = segment.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut previous_separator = false;
    let mut last = first;
    for character in characters {
        last = character;
        let separator = character == '_' || character == '-';
        if separator {
            // A separator may only join two words, so it cannot be first, last or
            // doubled — and `unknown__target` is two words with an empty one between
            // them, which is a typo rather than a name.
            if previous_separator {
                return false;
            }
            previous_separator = true;
            continue;
        }
        previous_separator = false;
        if !character.is_ascii_lowercase() && !character.is_ascii_digit() {
            return false;
        }
    }
    !previous_separator && (last.is_ascii_lowercase() || last.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_admits_the_vocabulary_and_nothing_else() {
        for value in [
            "build",
            "publish.github",
            "zup.manifest.unknown_target",
            "single",
            "universal",
            "thin",
            "offline",
            "x64",
            "build_1",
            "single-target",
            "zup.toolchain.component-mismatch",
        ] {
            assert!(is_identifier(value), "`{value}` should be an identifier");
        }
        for value in [
            "",
            "Build",
            "publish..github",
            "zup.manifest.",
            ".build",
            "zup/manifest",
            "build-",
            "-build",
            "build--one",
            "unknown__target",
            "build stage",
            "build\n",
            "trailing_",
        ] {
            assert!(!is_identifier(value), "`{value}` should be refused");
        }
    }

    /// Coming *in* from the wire, an ungrammatical value is refused rather than
    /// normalized: silently repairing it would hand a consumer a name zup never
    /// emitted, and a consumer that matches on names would never notice.
    #[test]
    fn a_wire_identifier_is_checked_in_both_directions() {
        let value: Identifier = serde_json::from_str("\"publish.github\"").expect("an identifier");
        assert!(value.is("publish.github"));
        let error = serde_json::from_str::<Identifier>("\"Not An Identifier\"").unwrap_err();
        assert!(error.to_string().contains("not an identifier"), "{error}");
    }
}
