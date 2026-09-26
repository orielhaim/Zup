use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum InstallLocation {
    Programs,
    UserData,
    SharedData,
    Menu,
    Desktop,
}

/// Every install location, in declaration order.
pub const INSTALL_LOCATIONS: [InstallLocation; 5] = [
    InstallLocation::Programs,
    InstallLocation::UserData,
    InstallLocation::SharedData,
    InstallLocation::Menu,
    InstallLocation::Desktop,
];

impl InstallLocation {
    /// The canonical name, which is also its wire form and the suffix of its
    /// `${location.*}` template variable.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Programs => "programs",
            Self::UserData => "user_data",
            Self::SharedData => "shared_data",
            Self::Menu => "menu",
            Self::Desktop => "desktop",
        }
    }

    /// Parse a canonical name, rejecting anything [`Self::as_str`] would not
    /// produce.
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
