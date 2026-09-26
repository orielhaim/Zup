//! Strongly typed logical identifiers.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::value::ValueError;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
        #[schemars(transparent)]
        pub struct $name(String);

        impl $name {
            /// Create an id from `value`, trimming surrounding whitespace.
            pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
                let trimmed = value.as_ref().trim();
                if trimmed.is_empty() {
                    return Err(ValueError::Empty { kind: $kind });
                }
                Ok(Self(trimmed.to_owned()))
            }

            /// Borrow the id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ValueError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValueError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type!(
    /// Stable logical application identifier.
    AppId,
    "app id"
);
id_type!(
    /// Stable logical component identifier.
    ComponentId,
    "component id"
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref();
        let mut bytes = value.bytes();
        let Some(first) = bytes.next() else {
            return Err(ValueError::Empty { kind: "plugin id" });
        };
        if !first.is_ascii_alphanumeric()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ValueError::InvalidPluginId {
                id: value.to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PluginId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for PluginId {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for PluginId {
    type Error = ValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for PluginId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

id_type!(
    /// Stable logical service identifier.
    ServiceId,
    "service id"
);
id_type!(
    /// Stable logical file-association identifier.
    FileAssociationId,
    "file association id"
);
id_type!(
    /// Stable identity for an opaque platform backend resource.
    BackendResourceId,
    "backend resource id"
);

/// URI scheme such as `acme` in `acme://`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct ProtocolScheme(String);

impl ProtocolScheme {
    /// Create a scheme from RFC 3986 syntax: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let scheme = value.as_ref().trim();
        if scheme.is_empty() {
            return Err(ValueError::Empty {
                kind: "protocol scheme",
            });
        }

        let mut chars = scheme.chars();
        let Some(first) = chars.next() else {
            return Err(ValueError::InvalidScheme {
                scheme: scheme.to_owned(),
            });
        };
        if !first.is_ascii_alphabetic()
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Err(ValueError::InvalidScheme {
                scheme: scheme.to_owned(),
            });
        }

        Ok(Self(scheme.to_owned()))
    }

    /// Borrow the scheme as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProtocolScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ProtocolScheme {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ProtocolScheme {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for ProtocolScheme {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProtocolScheme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Non-empty display name or label.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct NonEmptyString(String);

impl NonEmptyString {
    /// Create a label from `value`, trimming surrounding whitespace.
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let trimmed = value.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ValueError::Empty { kind: "name" });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Borrow the label as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NonEmptyString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for NonEmptyString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for NonEmptyString {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for NonEmptyString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NonEmptyString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}
