use std::fmt;

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use crate::condition::Condition;
use crate::digest::Sha256Digest;
use crate::ids::ValueError;
use crate::ids::{ComponentId, NonEmptyString};
use crate::model::Privilege;
use crate::path::RelativePath;
use crate::template::Template;

pub const MAX_PREREQUISITE_ID_BYTES: usize = 128;
pub const MAX_PREREQUISITE_ARGUMENTS: usize = 128;
pub const MAX_PREREQUISITE_ARGUMENT_BYTES: usize = 4096;
pub const MAX_PREREQUISITE_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RUNTIME_REQUIREMENT_ID_BYTES: usize = 128;
pub const MAX_INSTALLED_PACKAGE_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct PrerequisiteId(String);

impl PrerequisiteId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref().trim();
        if value.is_empty() || value.len() > MAX_PREREQUISITE_ID_BYTES {
            return Err(ValueError::Empty {
                kind: "prerequisite id",
            });
        }
        let mut bytes = value.bytes();
        let Some(first) = bytes.next() else {
            return Err(ValueError::Empty {
                kind: "prerequisite id",
            });
        };
        if !first.is_ascii_alphanumeric()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ValueError::InvalidPrerequisiteId {
                id: value.to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PrerequisiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PrerequisiteId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for PrerequisiteId {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for PrerequisiteId {
    type Error = ValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteArchitecture {
    X86,
    X64,
    Arm64,
    #[default]
    Current,
    Any,
}

impl PrerequisiteArchitecture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
            Self::Current => "current",
            Self::Any => "any",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct RuntimeRequirementId(String);

impl RuntimeRequirementId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > MAX_RUNTIME_REQUIREMENT_ID_BYTES {
            return Err(ValueError::Empty {
                kind: "runtime requirement id",
            });
        }
        for segment in value.split('.') {
            if segment.is_empty()
                || !segment.starts_with(|byte: char| byte.is_ascii_lowercase())
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                || segment.ends_with('_')
            {
                return Err(ValueError::InvalidRuntimeRequirementId {
                    id: value.to_owned(),
                });
            }
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuntimeRequirementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RuntimeRequirementId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for RuntimeRequirementId {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub id: RuntimeRequirementId,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub version: Option<VersionReq>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct InstalledPackageId(String);

impl InstalledPackageId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        let value = value.as_ref();
        if value.trim().is_empty() || value.len() > MAX_INSTALLED_PACKAGE_ID_BYTES {
            return Err(ValueError::Empty {
                kind: "installed package id",
            });
        }
        if value.trim() != value
            || value
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            || value.contains(['/', '\\', ':', '\0'])
        {
            return Err(ValueError::InvalidInstalledPackageId {
                id: value.to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstalledPackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for InstalledPackageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for InstalledPackageId {
    type Error = ValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InstalledPackage {
    pub id: InstalledPackageId,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub version: Option<VersionReq>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FileVersion {
    pub path: Template,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub version: Option<VersionReq>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
/// The portable semantic condition a prerequisite must satisfy.
///
/// Every variant names a fact the target system can be asked about. Detection
/// mechanics, package formats, and registry layout belong to the platform
/// provider, not to this model.
pub enum PrerequisiteRequirement {
    /// A runtime that must be present, optionally within a version range.
    Runtime(Runtime),
    /// An installed package that must be present, optionally within a version range.
    InstalledPackage(InstalledPackage),
    /// A file that must exist, optionally with a matching file version.
    FileVersion(FileVersion),
}

impl PrerequisiteRequirement {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Runtime(_) => "runtime",
            Self::InstalledPackage(_) => "installed_package",
            Self::FileVersion(_) => "file_version",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrerequisitePackage {
    Embedded {
        #[cfg_attr(feature = "schema", schemars(with = "String"))]
        path: RelativePath,
        #[cfg_attr(feature = "schema", schemars(with = "String"))]
        sha256: Sha256Digest,
        size: u64,
    },
    Remote {
        url: String,
        #[cfg_attr(feature = "schema", schemars(with = "String"))]
        sha256: Sha256Digest,
        #[serde(default)]
        size: Option<u64>,
        filename: String,
    },
}

impl PrerequisitePackage {
    pub const fn digest(&self) -> Sha256Digest {
        match self {
            Self::Embedded { sha256, .. } | Self::Remote { sha256, .. } => *sha256,
        }
    }

    pub const fn size(&self) -> Option<u64> {
        match self {
            Self::Embedded { size, .. } => Some(*size),
            Self::Remote { size, .. } => *size,
        }
    }

    pub fn filename(&self) -> &str {
        match self {
            Self::Embedded { path, .. } => path.file_name(),
            Self::Remote { filename, .. } => filename,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
/// How a prerequisite package is run once its requirement is unsatisfied.
///
/// The provider owns the command line it builds; the manifest only supplies
/// extra arguments, the accepted exit codes, and the privilege it needs.
pub struct PrerequisiteInstaller {
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default = "default_success_exit_codes")]
    pub success_exit_codes: Vec<i32>,
    #[serde(default = "default_reboot_exit_codes")]
    pub reboot_exit_codes: Vec<i32>,
    #[serde(default = "default_system_privilege")]
    pub privilege: Privilege,
}

impl Default for PrerequisiteInstaller {
    fn default() -> Self {
        Self {
            arguments: Vec::new(),
            success_exit_codes: default_success_exit_codes(),
            reboot_exit_codes: default_reboot_exit_codes(),
            privilege: Privilege::System,
        }
    }
}

fn default_success_exit_codes() -> Vec<i32> {
    vec![0]
}

fn default_reboot_exit_codes() -> Vec<i32> {
    vec![1641, 3010]
}

fn default_system_privilege() -> Privilege {
    Privilege::System
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Prerequisite {
    pub id: PrerequisiteId,
    pub name: NonEmptyString,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
    #[serde(default)]
    pub target: PrerequisiteArchitecture,
    pub requirement: PrerequisiteRequirement,
    pub package: PrerequisitePackage,
    #[serde(default)]
    pub installer: PrerequisiteInstaller,
}

impl Prerequisite {
    pub fn expected_digest(&self) -> Sha256Digest {
        self.package.digest()
    }
}
