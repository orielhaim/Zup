use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::TargetTriple;

use crate::format::ArtifactError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
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

    pub fn same_os(&self, other: &Self) -> bool {
        self.os == other.os
    }

    pub fn same_architecture(&self, other: &Self) -> bool {
        self.architecture == other.architecture
    }

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

/// supplied by a platform backend, so a portable model never has to name a
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostArchitecture {
    X86,
    X86_64,
    Arm,
    Arm64,
}

impl HostArchitecture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
            Self::Arm => "arm",
            Self::Arm64 => "aarch64",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "x86" | "i386" | "i586" | "i686" => Some(Self::X86),
            "x86_64" | "amd64" => Some(Self::X86_64),
            "arm" | "armv7" | "armv7a" | "thumbv7neon" => Some(Self::Arm),
            "aarch64" | "arm64" => Some(Self::Arm64),
            _ => None,
        }
    }

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
            "aarch64-apple-darwin",
        ] {
            let target = TargetTriple::parse(text).unwrap();
            assert_eq!(
                Platform::from_triple(&target).triple().unwrap(),
                target,
                "{text}"
            );
        }
        let linux = TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
        let platform = Platform::from_triple(&linux);
        assert_eq!(platform.vendor, None, "no vendor field, no vendor");
        assert_eq!(platform.abi.as_deref(), Some("gnu"), "the abi survives");
        assert_eq!(platform.triple().unwrap(), linux);
    }
}
