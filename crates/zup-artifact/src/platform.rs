//! Machine identity used to select a variant.
//!
//! A [`Platform`] is the portable view of a target triple: the machine identity
//! a selector matches on. `TargetTriple` remains the canonical identity of a
//! build, and a variant descriptor carries both, with the reader requiring them
//! to agree, so a platform can never drift from the target it came from.
//!
//! The platform's own components are the canonical spellings a target triple
//! normalizes to, which is what makes the round trip exact without this module
//! re-deriving a triple grammar. Selection itself is typed on the host side:
//! [`HostArchitecture`] is a closed set, and a host is compared against it, not
//! against a string.

use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::TargetTriple;

use crate::error::ArtifactError;

/// The machine identity a variant is built for and a host is compared against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    /// The operating system, as a canonical triple spells it.
    pub os: String,
    /// The machine architecture, as a canonical triple spells it.
    pub architecture: String,
    /// The vendor component, when the triple records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// The ABI or environment component, where the triple names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    /// An architecture variant component, where the triple names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    /// Derive the platform identity of a canonical target triple.
    pub fn from_triple(target: &TargetTriple) -> Self {
        let mut parts = target.as_str().split('-');
        let architecture = parts.next().unwrap_or_default().to_owned();
        let vendor = parts.next().map(str::to_owned);
        let os = parts.next().unwrap_or_default().to_owned();
        let environment = parts.next();
        let (abi, variant) = match environment {
            Some(environment) => (Some(environment.to_owned()), None),
            None => (None, None),
        };
        Self {
            os,
            architecture,
            vendor: vendor.filter(|vendor| vendor != "unknown"),
            abi,
            variant,
        }
    }

    /// Rebuild the canonical triple, or fail when the recorded components do not
    /// form one this model accepts.
    pub fn triple(&self) -> Result<TargetTriple, ArtifactError> {
        if self.os.is_empty() || self.architecture.is_empty() {
            return Err(ArtifactError::Invalid);
        }
        let mut canonical = self.architecture.clone();
        canonical.push('-');
        canonical.push_str(self.vendor.as_deref().unwrap_or("unknown"));
        canonical.push('-');
        canonical.push_str(&self.os);
        if let Some(abi) = &self.abi {
            canonical.push('-');
            canonical.push_str(abi);
        }
        if let Some(variant) = &self.variant {
            canonical.push('-');
            canonical.push_str(variant);
        }
        TargetTriple::parse(&canonical).map_err(|_| ArtifactError::Invalid)
    }

    /// Whether two platforms name the same operating system.
    pub fn same_os(&self, other: &Self) -> bool {
        self.os == other.os
    }

    /// Whether two platforms name the same machine architecture.
    pub fn same_architecture(&self, other: &Self) -> bool {
        self.architecture == other.architecture
    }

    /// Whether this platform names an architecture the host model knows.
    pub fn host_architecture(&self) -> Option<HostArchitecture> {
        HostArchitecture::from_name(&self.architecture)
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.os, self.architecture)?;
        if let Some(abi) = &self.abi {
            write!(formatter, "/{abi}")?;
        }
        Ok(())
    }
}

/// The machine architecture a host reports about itself.
///
/// A host reports its own architecture; the architectures it can additionally
/// execute through a compatibility or emulation layer are separate knowledge
/// supplied by a platform backend, so a portable model never has to name a
/// host-specific compatibility mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostArchitecture {
    X86,
    X86_64,
    Arm,
    Arm64,
}

impl HostArchitecture {
    /// The canonical triple spelling of this architecture.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
            Self::Arm => "arm",
            Self::Arm64 => "aarch64",
        }
    }

    /// The architecture a canonical triple spelling names, if this model knows
    /// it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "x86" | "i386" | "i586" | "i686" => Some(Self::X86),
            "x86_64" | "amd64" => Some(Self::X86_64),
            "arm" | "armv7" | "armv7a" | "thumbv7neon" => Some(Self::Arm),
            "aarch64" | "arm64" => Some(Self::Arm64),
            _ => None,
        }
    }

    /// The machine width of this architecture, used only to break a tie between
    /// two otherwise equal candidates.
    pub const fn width(self) -> u8 {
        match self {
            Self::X86 | Self::Arm => 1,
            Self::X86_64 | Self::Arm64 => 2,
        }
    }
}

impl fmt::Display for HostArchitecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_round_trips_through_the_canonical_triple() {
        for text in [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "i686-pc-windows-msvc",
        ] {
            let target = TargetTriple::parse(text).unwrap();
            let platform = Platform::from_triple(&target);
            assert_eq!(platform.triple().unwrap(), target, "{text}");
        }
    }

    #[test]
    fn a_triple_without_a_vendor_round_trips_through_the_default() {
        let target = TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
        let platform = Platform::from_triple(&target);
        assert_eq!(platform.vendor, None);
        assert_eq!(platform.abi.as_deref(), Some("gnu"));
        assert_eq!(platform.triple().unwrap(), target);
    }

    #[test]
    fn architecture_names_map_to_the_closed_host_set() {
        assert_eq!(
            HostArchitecture::from_name("x86_64"),
            Some(HostArchitecture::X86_64)
        );
        assert_eq!(
            HostArchitecture::from_name("aarch64"),
            Some(HostArchitecture::Arm64)
        );
        assert_eq!(HostArchitecture::from_name("riscv64"), None);
        for host in [
            HostArchitecture::X86,
            HostArchitecture::X86_64,
            HostArchitecture::Arm,
            HostArchitecture::Arm64,
        ] {
            assert_eq!(HostArchitecture::from_name(host.as_str()), Some(host));
        }
    }
}
