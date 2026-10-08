use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{PRESET_PROTOCOL_VERSION, ProductIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

impl From<Uuid> for SessionId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Components,
    Diagnostics,
    InstallDirectory,
    Launch,
    Maintenance,
    PlanPreview,
    Updates,
}

impl Capability {
    pub const ALL: &'static [Self] = &[
        Self::Components,
        Self::Diagnostics,
        Self::InstallDirectory,
        Self::Launch,
        Self::Maintenance,
        Self::PlanPreview,
        Self::Updates,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Components => "components",
            Self::Diagnostics => "diagnostics",
            Self::InstallDirectory => "install-directory",
            Self::Launch => "launch",
            Self::Maintenance => "maintenance",
            Self::PlanPreview => "plan-preview",
            Self::Updates => "updates",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|known| known.as_str() == name)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = UnknownCapability;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| UnknownCapability(value.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a UI capability this protocol defines")]
pub struct UnknownCapability(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(BTreeSet<Capability>);

impl Capabilities {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn with(mut self, capability: Capability) -> Self {
        self.0.insert(capability);
        self
    }

    pub fn without(mut self, capability: Capability) -> Self {
        self.0.remove(&capability);
        self
    }

    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> Vec<&'static str> {
        Capability::ALL
            .iter()
            .filter(|capability| self.0.contains(capability))
            .map(|capability| capability.as_str())
            .collect()
    }

    pub fn missing(&self, required: &Capabilities) -> Vec<&'static str> {
        Capability::ALL
            .iter()
            .filter(|capability| required.contains(**capability) && !self.contains(**capability))
            .map(|capability| capability.as_str())
            .collect()
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self::new(iter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetHello {
    pub protocol_version: u32,
    pub session: SessionId,
    pub preset: String,
    pub preset_version: String,
    pub required_capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    pub protocol_version: u32,
    pub session: SessionId,
    pub capabilities: Capabilities,
    pub product: ProductIdentity,
    pub host_version: String,
}

impl HostHello {
    pub fn new(
        session: SessionId,
        capabilities: Capabilities,
        product: ProductIdentity,
        host_version: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: PRESET_PROTOCOL_VERSION,
            session,
            capabilities,
            product,
            host_version: host_version.into(),
        }
    }
}
