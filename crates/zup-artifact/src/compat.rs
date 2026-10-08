use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{App, Frontend, Install, Sha256Digest, TargetTriple, Template};

use crate::platform::Platform;
use crate::variant::{
    DistributionVariant, HostVersion, MinimumHost, PlatformCapability, PlatformOs,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherSubsystem {
    Gui,
    Console,
}

impl LauncherSubsystem {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Console => "console",
        }
    }
}

impl fmt::Display for LauncherSubsystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub const fn frontend_subsystem(frontend: Frontend) -> LauncherSubsystem {
    match frontend {
        Frontend::Gui => LauncherSubsystem::Gui,
        Frontend::Console | Frontend::Headless => LauncherSubsystem::Console,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityDimension {
    Platform,
    ArtifactBackend,
    LauncherSubsystem,
    ApplicationIdentity,
    UpdateTrust,
    InstallerSemantics,
}

impl CompatibilityDimension {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::ArtifactBackend => "artifact backend",
            Self::LauncherSubsystem => "launcher subsystem",
            Self::ApplicationIdentity => "application identity",
            Self::UpdateTrust => "update trust",
            Self::InstallerSemantics => "installer semantics",
        }
    }

    pub const fn requirement(self) -> &'static str {
        match self {
            Self::Platform => "one artifact serves one operating system",
            Self::ArtifactBackend => "one artifact uses one artifact backend",
            Self::LauncherSubsystem => "one artifact is one launcher experience",
            Self::ApplicationIdentity => "one artifact carries one application version",
            Self::UpdateTrust => "one artifact carries one update trust configuration",
            Self::InstallerSemantics => {
                "one artifact installs one scope, one destination, and one component set"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "dimension", rename_all = "snake_case")]
pub enum Incompatibility {
    Platform {
        left: Platform,
        right: Platform,
    },
    ArtifactBackend {
        left: String,
        right: String,
    },
    LauncherSubsystem {
        left: LauncherSubsystem,
        right: LauncherSubsystem,
    },
    ApplicationIdentity {
        left: String,
        right: String,
        detail: String,
    },
    UpdateTrust {
        left: String,
        right: String,
    },
    InstallerSemantics {
        detail: String,
    },
}

impl Incompatibility {
    pub fn dimension(&self) -> CompatibilityDimension {
        match self {
            Self::Platform { .. } => CompatibilityDimension::Platform,
            Self::ArtifactBackend { .. } => CompatibilityDimension::ArtifactBackend,
            Self::LauncherSubsystem { .. } => CompatibilityDimension::LauncherSubsystem,
            Self::ApplicationIdentity { .. } => CompatibilityDimension::ApplicationIdentity,
            Self::UpdateTrust { .. } => CompatibilityDimension::UpdateTrust,
            Self::InstallerSemantics { .. } => CompatibilityDimension::InstallerSemantics,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Platform { left, right } => {
                format!("`{left}` and `{right}` are different operating systems")
            }
            Self::ArtifactBackend { left, right } => {
                format!("`{left}` and `{right}` use different artifact backends")
            }
            Self::LauncherSubsystem { left, right } => {
                format!("`{left}` and `{right}` are different launcher subsystems")
            }
            Self::ApplicationIdentity { detail, .. } => detail.clone(),
            Self::UpdateTrust { left, right } => {
                format!("`{left}` and `{right}` declare different update trust")
            }
            Self::InstallerSemantics { detail } => detail.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[error("`{left}` and `{right}` cannot form one artifact ({}): {}", .reason.dimension().as_str(), .reason.detail())]
pub struct Incompatible {
    pub left: String,
    pub right: String,
    pub reason: Box<Incompatibility>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantShape {
    pub id: String,
    pub platform: Platform,
    pub target: TargetTriple,
    pub subsystem: LauncherSubsystem,
    pub application: App,
    pub install: Install,
    pub update_trust: Option<String>,
    pub components: Vec<String>,
}

impl VariantShape {
    fn of(variant: &DistributionVariant) -> Self {
        Self {
            id: variant.id().to_owned(),
            platform: variant.platform().clone(),
            target: variant.target().clone(),
            subsystem: variant.subsystem(),
            application: variant.application().clone(),
            install: variant.install().clone(),
            update_trust: variant.plan().installer.updates.as_ref().map(|updates| {
                format!(
                    "{}#{}#{}",
                    updates.repository,
                    updates.channel,
                    Sha256Digest::from_bytes(Sha256::digest(&updates.trusted_root).into())
                )
            }),
            components: variant
                .plan()
                .installer
                .components
                .iter()
                .map(|component| component.id.to_string())
                .collect(),
        }
    }
}

pub fn check_compatibility(variants: &[&DistributionVariant]) -> Result<(), Incompatible> {
    let shapes = variants
        .iter()
        .map(|variant| VariantShape::of(variant))
        .collect::<Vec<_>>();
    for (index, left) in shapes.iter().enumerate() {
        for right in shapes.iter().skip(index + 1) {
            if let Some(reason) = compare(left, right) {
                return Err(Incompatible {
                    left: left.id.clone(),
                    right: right.id.clone(),
                    reason: Box::new(reason),
                });
            }
        }
    }
    Ok(())
}

fn compare(left: &VariantShape, right: &VariantShape) -> Option<Incompatibility> {
    if !left.platform.same_os(&right.platform) {
        return Some(Incompatibility::Platform {
            left: left.platform.clone(),
            right: right.platform.clone(),
        });
    }
    if left.subsystem != right.subsystem {
        return Some(Incompatibility::LauncherSubsystem {
            left: left.subsystem,
            right: right.subsystem,
        });
    }
    if let Some(reason) = compare_application(&left.application, &right.application) {
        return Some(reason);
    }
    if left.update_trust != right.update_trust {
        return Some(Incompatibility::UpdateTrust {
            left: left.update_trust.clone().unwrap_or_else(|| "none".into()),
            right: right.update_trust.clone().unwrap_or_else(|| "none".into()),
        });
    }
    if let Some(detail) = compare_install(
        &left.install,
        &right.install,
        &left.components,
        &right.components,
    ) {
        return Some(Incompatibility::InstallerSemantics { detail });
    }
    None
}

fn compare_application(left: &App, right: &App) -> Option<Incompatibility> {
    if left.id == right.id && left.name == right.name && left.version == right.version {
        return None;
    }
    Some(Incompatibility::ApplicationIdentity {
        left: format!("{} {}", left.name, left.version),
        right: format!("{} {}", right.name, right.version),
        detail: format!(
            "`{} {}` and `{} {}` are different application versions",
            left.name, left.version, right.name, right.version
        ),
    })
}

fn compare_install(
    left: &Install,
    right: &Install,
    left_components: &[String],
    right_components: &[String],
) -> Option<String> {
    if left.scope != right.scope {
        return Some(format!(
            "install scope differs (`{}` and `{}`)",
            left.scope, right.scope
        ));
    }
    if left.allow_directory_override != right.allow_directory_override {
        return Some("install directory override policy differs".into());
    }
    let user = (left.directory.user.as_ref(), right.directory.user.as_ref());
    let machine = (
        left.directory.machine.as_ref(),
        right.directory.machine.as_ref(),
    );
    if user.0.map(Template::to_string) != user.1.map(Template::to_string)
        || machine.0.map(Template::to_string) != machine.1.map(Template::to_string)
    {
        return Some("install destination templates differ".into());
    }
    if left_components != right_components {
        return Some(format!(
            "component sets differ (`{}` and `{}`); a shared installer would present one of them to a machine that cannot use it",
            left_components.join(", "),
            right_components.join(", ")
        ));
    }
    None
}

pub fn satisfies_minimum_host(
    minimum: Option<&MinimumHost>,
    host_os: Option<PlatformOs>,
    host_version: Option<&HostVersion>,
) -> bool {
    let Some(minimum) = minimum else {
        return true;
    };
    if let Some(host_os) = host_os
        && host_os != minimum.os
    {
        return false;
    }
    match host_version {
        Some(version) => *version >= minimum.version,
        None => false,
    }
}

pub fn requires_native(capabilities: &[PlatformCapability]) -> bool {
    capabilities.contains(&PlatformCapability::MachineComponents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimum_host_version_fails_closed_when_the_host_is_unknown() {
        let minimum = MinimumHost {
            os: crate::variant::PlatformOs::Windows,
            version: HostVersion::new(10, 0, 22000),
        };
        assert!(satisfies_minimum_host(
            Some(&minimum),
            Some(PlatformOs::Windows),
            Some(&HostVersion::new(11, 0, 0))
        ));
        assert!(!satisfies_minimum_host(
            Some(&minimum),
            Some(PlatformOs::Windows),
            Some(&HostVersion::new(10, 0, 17763))
        ));
        assert!(!satisfies_minimum_host(
            Some(&minimum),
            Some(PlatformOs::Windows),
            None
        ));
        assert!(!satisfies_minimum_host(
            Some(&minimum),
            Some(PlatformOs::Linux),
            Some(&HostVersion::new(99, 0, 0))
        ));
        assert!(satisfies_minimum_host(None, None, None));
    }
}
