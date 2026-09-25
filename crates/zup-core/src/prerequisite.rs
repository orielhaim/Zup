use std::fmt;

use schemars::JsonSchema;
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use crate::condition::Condition;
use crate::digest::Sha256Digest;
use crate::ids::{ComponentId, NonEmptyString};
use crate::model::Privilege;
use crate::path::RelativePath;
use crate::template::Template;
use crate::value::ValueError;

pub const MAX_PREREQUISITE_ID_BYTES: usize = 128;
pub const MAX_PREREQUISITE_ARGUMENTS: usize = 128;
pub const MAX_PREREQUISITE_ARGUMENT_BYTES: usize = 4096;
pub const MAX_PREREQUISITE_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[schemars(transparent)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHive {
    #[default]
    CurrentUser,
    LocalMachine,
    ClassesRoot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrerequisiteDetector {
    VisualCppV14 {
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
    },
    #[serde(rename = "dotnet_runtime", alias = "dot_net_runtime")]
    DotNetRuntime {
        #[serde(default)]
        desktop: bool,
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
    },
    #[serde(rename = "webview2_evergreen", alias = "web_view2_evergreen")]
    WebView2Evergreen {
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
    },
    MsiProduct {
        product_code: String,
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
    },
    RegistryValue {
        #[serde(default)]
        hive: RegistryHive,
        key: String,
        value: String,
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
        #[serde(default)]
        expected: Option<String>,
    },
    FileVersion {
        path: Template,
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        version: Option<VersionReq>,
    },
}

impl PrerequisiteDetector {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::VisualCppV14 { .. } => "visual_cpp_v14",
            Self::DotNetRuntime { .. } => "dotnet_runtime",
            Self::WebView2Evergreen { .. } => "webview2_evergreen",
            Self::MsiProduct { .. } => "msi_product",
            Self::RegistryValue { .. } => "registry_value",
            Self::FileVersion { .. } => "file_version",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrerequisitePackage {
    Embedded {
        #[schemars(with = "String")]
        path: RelativePath,
        #[schemars(with = "String")]
        sha256: Sha256Digest,
        size: u64,
    },
    Remote {
        url: String,
        #[schemars(with = "String")]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteInstallerKind {
    #[default]
    Exe,
    Msi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteInstaller {
    #[serde(default)]
    pub kind: PrerequisiteInstallerKind,
    #[serde(default, alias = "args")]
    pub arguments: Vec<String>,
    #[serde(default = "default_success_exit_codes")]
    pub success_exit_codes: Vec<i32>,
    #[serde(default = "default_reboot_exit_codes")]
    pub reboot_exit_codes: Vec<i32>,
    #[serde(default = "default_machine_privilege")]
    pub privilege: Privilege,
}

impl Default for PrerequisiteInstaller {
    fn default() -> Self {
        Self {
            kind: PrerequisiteInstallerKind::Exe,
            arguments: Vec::new(),
            success_exit_codes: default_success_exit_codes(),
            reboot_exit_codes: default_reboot_exit_codes(),
            privilege: Privilege::Machine,
        }
    }
}

fn default_success_exit_codes() -> Vec<i32> {
    vec![0]
}

fn default_reboot_exit_codes() -> Vec<i32> {
    vec![1641, 3010]
}

fn default_machine_privilege() -> Privilege {
    Privilege::Machine
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    #[serde(alias = "detect")]
    pub detector: PrerequisiteDetector,
    #[serde(alias = "source", alias = "artifact")]
    pub package: PrerequisitePackage,
    #[serde(default, alias = "install")]
    pub installer: PrerequisiteInstaller,
}

impl Prerequisite {
    pub fn expected_digest(&self) -> Sha256Digest {
        self.package.digest()
    }
}
