use std::fmt;

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
        #[cfg_attr(feature = "schema", schemars(transparent))]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
                let trimmed = value.as_ref().trim();
                if trimmed.is_empty() {
                    return Err(ValueError::Empty { kind: $kind });
                }
                Ok(Self(trimmed.to_owned()))
            }

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

id_type!(AppId, "app id");
id_type!(ComponentId, "component id");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
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

id_type!(ServiceId, "service id");
id_type!(FileAssociationId, "file association id");
id_type!(BackendResourceId, "backend resource id");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct ProtocolScheme(String);

impl ProtocolScheme {
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct NonEmptyString(String);

impl NonEmptyString {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let trimmed = value.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ValueError::Empty { kind: "name" });
        }
        Ok(Self(trimmed.to_owned()))
    }

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

use base64::Engine;

pub fn base64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(input)
}

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },

    #[error("invalid URI scheme `{scheme}`")]
    InvalidScheme { scheme: String },

    #[error("invalid file extension `{extension}`")]
    InvalidExtension { extension: String },

    #[error("invalid plugin id `{id}`")]
    InvalidPluginId { id: String },

    #[error("invalid prerequisite id `{id}`")]
    InvalidPrerequisiteId { id: String },

    #[error("invalid runtime requirement id `{id}`")]
    InvalidRuntimeRequirementId { id: String },

    #[error("invalid installed package id `{id}`")]
    InvalidInstalledPackageId { id: String },
}

use crate::model::{FileExtension, LauncherLocation};

/// matching. Never use runtime UUIDs here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKey {
    Maintenance {
        app_id: String,
        version: String,
        destination: String,
    },
    Backend {
        id: BackendResourceId,
    },
    File {
        destination: String,
    },
    Launcher {
        location: LauncherLocation,
        name: String,
    },
    PathEntry {
        value: String,
    },
    Service {
        id: ServiceId,
    },
    Protocol {
        scheme: ProtocolScheme,
    },
    FileAssociation {
        id: FileAssociationId,
    },
    FileAssociationExtension {
        extension: FileExtension,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename_all = "snake_case"))]
pub enum InstallLocation {
    Programs,
    UserData,
    SharedData,
    Menu,
    Desktop,
}

pub const INSTALL_LOCATIONS: [InstallLocation; 5] = [
    InstallLocation::Programs,
    InstallLocation::UserData,
    InstallLocation::SharedData,
    InstallLocation::Menu,
    InstallLocation::Desktop,
];

impl InstallLocation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Programs => "programs",
            Self::UserData => "user_data",
            Self::SharedData => "shared_data",
            Self::Menu => "menu",
            Self::Desktop => "desktop",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        INSTALL_LOCATIONS
            .into_iter()
            .find(|location| location.as_str() == name)
    }
}

impl fmt::Display for InstallLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for InstallLocation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InstallLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown install location `{raw}`; expected one of {}",
                INSTALL_LOCATIONS
                    .iter()
                    .map(|location| location.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct TransactionId(pub uuid::Uuid);
impl TransactionId {
    pub fn new_v7() -> Self {
        Self(uuid::Uuid::now_v7())
    }
    pub fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(self) -> uuid::Uuid {
        self.0
    }
}
impl std::fmt::Display for TransactionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::str::FromStr for TransactionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<uuid::Uuid>().map(Self)
    }
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionId(pub uuid::Uuid);
impl SessionId {
    pub fn new_v7() -> Self {
        Self(uuid::Uuid::now_v7())
    }
    pub fn parse(s: &str) -> Result<Self, uuid::Error> {
        s.parse::<uuid::Uuid>().map(Self)
    }
}
impl std::str::FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}
impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
