//! Session identity, capabilities, and the handshake.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ProductIdentity, UI_PROTOCOL_VERSION};

/// Identity of one host ↔ preset relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UiSessionId(pub Uuid);

impl UiSessionId {
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Display for UiSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One independently evolvable piece of the contract.
///
/// A capability marks something a host can add or withdraw on its own
/// schedule. What a preset cannot render without is not a capability; that is
/// the protocol version, which a preset states in its hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiCapability {
    /// The application has components a person may choose between.
    Components,
    /// The host can reveal the session log and produce a diagnostic summary.
    Diagnostics,
    /// The application allows choosing an install location.
    InstallDirectory,
    /// The host can start the application it installed.
    Launch,
    /// The host can modify, repair, and uninstall an existing installation.
    Maintenance,
    /// The host keeps a plan of what the current choices would change.
    PlanPreview,
    /// The application is configured for updates.
    Updates,
}

impl UiCapability {
    /// Every capability, in a stable order.
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

    /// Parse a capability name, or `None` for one this version does not define.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|known| known.as_str() == name)
    }
}

impl fmt::Display for UiCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for UiCapability {
    type Err = UnknownCapability;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| UnknownCapability(value.to_owned()))
    }
}

/// A capability name this protocol version does not define.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a UI capability this protocol defines")]
pub struct UnknownCapability(pub String);

/// The capabilities one side has.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UiCapabilities(BTreeSet<UiCapability>);

impl UiCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = UiCapability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    /// Add one capability, returning the set.
    pub fn with(mut self, capability: UiCapability) -> Self {
        self.0.insert(capability);
        self
    }

    /// Remove one capability, returning the set.
    pub fn without(mut self, capability: UiCapability) -> Self {
        self.0.remove(&capability);
        self
    }

    pub fn contains(&self, capability: UiCapability) -> bool {
        self.0.contains(&capability)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The capability names, in the order [`UiCapability::ALL`] declares them.
    pub fn names(&self) -> Vec<&'static str> {
        UiCapability::ALL
            .iter()
            .filter(|capability| self.0.contains(capability))
            .map(|capability| capability.as_str())
            .collect()
    }

    /// The required capabilities this set does not provide.
    pub fn missing(&self, required: &UiCapabilities) -> Vec<&'static str> {
        UiCapability::ALL
            .iter()
            .filter(|capability| required.contains(**capability) && !self.contains(**capability))
            .map(|capability| capability.as_str())
            .collect()
    }
}

impl FromIterator<UiCapability> for UiCapabilities {
    fn from_iter<I: IntoIterator<Item = UiCapability>>(iter: I) -> Self {
        Self::new(iter)
    }
}

/// The preset's first message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiHello {
    pub protocol_version: u32,
    pub session: UiSessionId,
    /// The preset's own name, so a host log says which preset it was driving.
    pub preset: String,
    pub preset_version: String,
    pub required_capabilities: UiCapabilities,
}

/// The host's answer, and the identity the preset is presenting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    pub protocol_version: u32,
    pub session: UiSessionId,
    pub capabilities: UiCapabilities,
    pub product: ProductIdentity,
    /// The engine version, for a preset that reports it in a diagnostics summary.
    pub host_version: String,
}

impl HostHello {
    pub fn new(
        session: UiSessionId,
        capabilities: UiCapabilities,
        product: ProductIdentity,
        host_version: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: UI_PROTOCOL_VERSION,
            session,
            capabilities,
            product,
            host_version: host_version.into(),
        }
    }
}
