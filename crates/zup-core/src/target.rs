use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

#[cfg(feature = "schema")]
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use target_lexicon::{
    Aarch64Architecture, Architecture, OperatingSystem, Triple, X86_32Architecture,
};
use thiserror::Error;

use crate::ids::ValueError;
use crate::model::{Frontend, Install, Source};
use crate::template::Template;

pub use target_lexicon::{
    Architecture as TargetArchitecture, OperatingSystem as TargetOperatingSystem,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetParseError {
    #[error(
        "unknown target `{target}`: expected `windows-x64`, `windows-x86`, `windows-arm64`, `linux-x64` or `linux-arm64`"
    )]
    Unknown { target: String },
    #[error("unsupported target `{target}`: zup builds installers for Windows and Linux")]
    Unsupported { target: String },
    #[error("target `{target}` needs an architecture: use `{suggestion}`")]
    MissingArchitecture { target: String, suggestion: String },
}

struct TargetDef {
    name: &'static str,
    triple: &'static str,
    architecture: Architecture,
    operating_system: OperatingSystem,
    musl: bool,
}

const TARGETS: &[TargetDef] = &[
    TargetDef {
        name: "windows-x64",
        triple: "x86_64-pc-windows-msvc",
        architecture: Architecture::X86_64,
        operating_system: OperatingSystem::Windows,
        musl: false,
    },
    TargetDef {
        name: "windows-x86",
        triple: "i686-pc-windows-msvc",
        architecture: Architecture::X86_32(X86_32Architecture::I686),
        operating_system: OperatingSystem::Windows,
        musl: false,
    },
    TargetDef {
        name: "windows-arm64",
        triple: "aarch64-pc-windows-msvc",
        architecture: Architecture::Aarch64(Aarch64Architecture::Aarch64),
        operating_system: OperatingSystem::Windows,
        musl: false,
    },
    TargetDef {
        name: "linux-x64",
        triple: "x86_64-unknown-linux-gnu",
        architecture: Architecture::X86_64,
        operating_system: OperatingSystem::Linux,
        musl: false,
    },
    TargetDef {
        name: "linux-arm64",
        triple: "aarch64-unknown-linux-gnu",
        architecture: Architecture::Aarch64(Aarch64Architecture::Aarch64),
        operating_system: OperatingSystem::Linux,
        musl: false,
    },
    TargetDef {
        name: "linux-x64-musl",
        triple: "x86_64-unknown-linux-musl",
        architecture: Architecture::X86_64,
        operating_system: OperatingSystem::Linux,
        musl: true,
    },
    TargetDef {
        name: "linux-arm64-musl",
        triple: "aarch64-unknown-linux-musl",
        architecture: Architecture::Aarch64(Aarch64Architecture::Aarch64),
        operating_system: OperatingSystem::Linux,
        musl: true,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct TargetProfileId(String);

impl TargetProfileId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(ValueError::Empty {
                kind: "target profile id",
            });
        }
        Ok(Self(value.to_owned()))
    }

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
/// Build payload declared for one target profile.
///
/// `target` is optional: a profile named for a Zup target, such as
/// `[build.targets.windows-x64]`, infers it from its own name. A profile with
/// any other name states it explicitly with `target = "windows-x64"`.
pub struct TargetProfile {
    #[serde(default)]
    pub target: Option<Target>,
    pub source: Source,
    #[serde(default)]
    pub frontend: Option<Frontend>,
    #[serde(default)]
    pub install: Option<Install>,
}

impl TargetProfile {
    pub fn target(&self, profile: &TargetProfileId) -> Result<Target, TargetParseError> {
        match self.target {
            Some(target) => Ok(target),
            None => Target::parse(profile.as_str()),
        }
    }
}

/// place in `zup-manifest`, so a caller can never mix precedence rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetOverrides {
    pub source: Option<Source>,
    pub install_directory: Option<Template>,
    pub frontend: Option<Frontend>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResolvedTargetConfig {
    pub profile: TargetProfileId,
    pub target: Target,
    pub source: Source,
    pub frontend: Frontend,
    pub install: Install,
}

/// A Zup target: an operating system and an architecture, named operating
/// system first.
///
/// The name is the whole public identity: `windows-x64`, `linux-arm64`. The
/// Rust triple it compiles to is available through [`Target::rust_triple`]
/// for the build tooling that needs it, and never appears in manifests,
/// diagnostics, or file names.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    index: u8,
}

impl Target {
    pub const WINDOWS_X64: Self = Self { index: 0 };
    pub const WINDOWS_X86: Self = Self { index: 1 };
    pub const WINDOWS_ARM64: Self = Self { index: 2 };
    pub const LINUX_X64: Self = Self { index: 3 };
    pub const LINUX_ARM64: Self = Self { index: 4 };
    pub const LINUX_X64_MUSL: Self = Self { index: 5 };
    pub const LINUX_ARM64_MUSL: Self = Self { index: 6 };

    pub fn parse(value: impl AsRef<str>) -> Result<Self, TargetParseError> {
        let target = value.as_ref();
        let normalized = target.trim().to_ascii_lowercase();
        if let Some(found) = parse_name(&normalized).or_else(|| parse_triple(&normalized)) {
            return Ok(found);
        }
        if matches!(normalized.as_str(), "windows" | "win") {
            return Err(TargetParseError::MissingArchitecture {
                target: target.to_owned(),
                suggestion: "windows-x64".to_owned(),
            });
        }
        if matches!(normalized.as_str(), "linux" | "lnx") {
            return Err(TargetParseError::MissingArchitecture {
                target: target.to_owned(),
                suggestion: "linux-x64".to_owned(),
            });
        }
        let probe = normalized
            .replacen("x64-", "x86_64-", 1)
            .replacen("arm64-", "aarch64-", 1);
        if let Ok(triple) = probe.parse::<Triple>()
            && triple.architecture != Architecture::Unknown
            && triple.operating_system != OperatingSystem::Unknown
        {
            return Err(TargetParseError::Unsupported {
                target: target.to_owned(),
            });
        }
        Err(TargetParseError::Unknown {
            target: target.to_owned(),
        })
    }

    /// The supported target matching the host OS, architecture, and C runtime.
    pub fn host() -> Result<Self, TargetParseError> {
        let triple = Triple::host();
        Self::parse(triple.to_string())
    }

    fn def(self) -> &'static TargetDef {
        &TARGETS[self.index as usize]
    }

    /// The canonical Zup name: `windows-x64`.
    pub fn as_str(self) -> &'static str {
        self.def().name
    }

    /// The Rust triple a compiler expects for this target.
    pub fn rust_triple(self) -> &'static str {
        self.def().triple
    }

    pub fn architecture(self) -> Architecture {
        self.def().architecture
    }

    pub fn operating_system(self) -> OperatingSystem {
        self.def().operating_system
    }

    pub const fn is_windows(self) -> bool {
        matches!(self.index, 0..=2)
    }

    pub const fn is_linux(self) -> bool {
        !self.is_windows()
    }

    /// The executable suffix for installers built for this target.
    pub fn executable_suffix(self) -> &'static str {
        executable_suffix(self.operating_system())
    }
}

fn parse_name(normalized: &str) -> Option<Target> {
    let mut parts = normalized.split('-');
    let os = match parts.next()? {
        "windows" | "win" => OperatingSystem::Windows,
        "linux" | "lnx" => OperatingSystem::Linux,
        _ => return None,
    };
    let architecture = match parts.next()? {
        "x64" | "x86_64" | "amd64" => Architecture::X86_64,
        "x86" | "i686" | "i386" => Architecture::X86_32(X86_32Architecture::I686),
        "arm64" | "aarch64" => Architecture::Aarch64(Aarch64Architecture::Aarch64),
        _ => return None,
    };
    let environment = match parts.next() {
        None => None,
        Some("msvc") if os == OperatingSystem::Windows => None,
        Some("gnu") if os == OperatingSystem::Linux => None,
        Some("musl") if os == OperatingSystem::Linux => Some("musl"),
        Some(_) => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    TARGETS
        .iter()
        .position(|def| {
            def.operating_system == os
                && def.architecture == architecture
                && def.musl == environment.is_some()
        })
        .map(|index| Target { index: index as u8 })
}

fn parse_triple(normalized: &str) -> Option<Target> {
    let probe = normalized
        .replacen("x64-", "x86_64-", 1)
        .replacen("arm64-", "aarch64-", 1);
    TARGETS
        .iter()
        .position(|def| def.triple == probe)
        .map(|index| Target { index: index as u8 })
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Target({})", self.as_str())
    }
}

pub const fn executable_suffix(os: OperatingSystem) -> &'static str {
    match os {
        OperatingSystem::Windows => ".exe",
        _ => "",
    }
}

pub fn host_architecture() -> String {
    target_lexicon::Architecture::host().to_string()
}

pub fn host_operating_system() -> String {
    target_lexicon::OperatingSystem::host().to_string()
}

impl PartialOrd for Target {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Target {
    fn cmp(&self, other: &Self) -> Ordering {
        self.index.cmp(&other.index)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Target {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for Target {
    type Err = TargetParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for Target {
    type Error = TargetParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for Target {
    type Error = TargetParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl JsonSchema for Target {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Target".into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        "zup_core::Target".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A Zup target such as `windows-x64` or `linux-x64`.",
            "examples": ["windows-x64", "windows-arm64", "linux-x64"]
        })
    }
}
