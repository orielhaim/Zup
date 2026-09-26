use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use target_lexicon::{Architecture, OperatingSystem, ParseError, Triple};
use thiserror::Error;

use crate::model::{Frontend, Install, Source};
use crate::template::Template;
use crate::value::ValueError;

pub use target_lexicon::{
    Architecture as TargetArchitecture, OperatingSystem as TargetOperatingSystem,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetParseError {
    #[error("invalid target triple `{target}`: {source}")]
    Invalid {
        target: String,
        #[source]
        source: ParseError,
    },
    #[error("invalid target triple `{target}`: architecture and operating system must be known")]
    UnknownIdentity { target: String },
}

/// Stable, human-readable identity for a target profile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct TargetProfileId(String);

impl TargetProfileId {
    /// Create a target profile id, trimming surrounding whitespace.
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(ValueError::Empty {
                kind: "target profile id",
            });
        }
        Ok(Self(value.to_owned()))
    }

    /// Borrow the target profile id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for TargetProfileId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TargetProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TargetProfileId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for TargetProfileId {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for TargetProfileId {
    type Error = ValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for TargetProfileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TargetProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Build payload declared for one target profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetProfile {
    pub target: TargetTriple,
    pub source: Source,
    #[serde(default)]
    pub frontend: Option<Frontend>,
    #[serde(default)]
    pub install: Option<Install>,
}

/// Non-serialized caller overrides applied while resolving one target profile.
///
/// A field is `Some` only when the caller supplied one. Each field resolves
/// independently, in the order caller override, then the profile's own
/// declaration, then the common manifest. Resolution belongs to exactly one
/// place in `zup-manifest`, so a caller can never mix precedence rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetOverrides {
    /// Replaces the profile's build source directory.
    pub source: Option<Source>,
    /// Replaces the install directory template of every scope this target
    /// installs to. The target's scope and directory-override policy are
    /// unaffected.
    pub install_directory: Option<Template>,
    /// Replaces the profile's frontend.
    pub frontend: Option<Frontend>,
}

/// One selected target profile with authoring overrides resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedTargetConfig {
    pub profile: TargetProfileId,
    pub target: TargetTriple,
    pub source: Source,
    pub frontend: Frontend,
    pub install: Install,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct TargetTriple {
    canonical: String,
}

impl TargetTriple {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, TargetParseError> {
        let target = value.as_ref();
        let normalized = target
            .strip_prefix("x64-")
            .map(|suffix| format!("x86_64-{suffix}"))
            .unwrap_or_else(|| target.to_owned());
        let triple = normalized
            .parse::<Triple>()
            .map_err(|source| TargetParseError::Invalid {
                target: target.to_owned(),
                source,
            })?;
        if triple.architecture == Architecture::Unknown
            || triple.operating_system == OperatingSystem::Unknown
        {
            return Err(TargetParseError::UnknownIdentity {
                target: target.to_owned(),
            });
        }

        Ok(Self {
            canonical: triple.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    pub fn as_lexicon(&self) -> Triple {
        self.canonical
            .parse()
            .expect("TargetTriple stores only canonical targets")
    }

    pub fn architecture(&self) -> Architecture {
        self.as_lexicon().architecture
    }

    pub fn operating_system(&self) -> OperatingSystem {
        self.as_lexicon().operating_system
    }
}

impl PartialOrd for TargetTriple {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TargetTriple {
    fn cmp(&self, other: &Self) -> Ordering {
        self.canonical.cmp(&other.canonical)
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl AsRef<str> for TargetTriple {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for TargetTriple {
    type Err = TargetParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for TargetTriple {
    type Error = TargetParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for TargetTriple {
    type Error = TargetParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for TargetTriple {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical)
    }
}

impl<'de> Deserialize<'de> for TargetTriple {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}
