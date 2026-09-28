//! What a machine needs to know about itself to choose one variant.
//!
//! A release graph names several variants. Choosing between them is a decision
//! about the *host*, and the engine cannot make it: it has no opinion about what
//! a processor is. So the host describes itself as data, and selection is a pure
//! function of that description and the release's own claims.
//!
//! Keeping it data is what lets the thin bootstrapper, the updater inside an
//! installed copy, and a test all make the same choice without three copies of
//! the rules. It is also the reason a selection can be *explained*: a refusal
//! names the requirement the host did not meet rather than an opaque
//! incompatibility.

use serde::{Deserialize, Serialize};
use zup_core::TargetTriple;

use crate::release::{ReleaseDescriptor, ReleaseVariant};

/// The processor families a host may report.
///
/// This is a closed set rather than a target triple, because a triple carries
/// an ABI and a vendor that decide nothing about whether the machine can *run*
/// the variant; a variant's own target carries those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostArchitecture {
    X86,
    X86_64,
    Arm64,
}

impl HostArchitecture {
    /// The spelling a target triple uses for this architecture.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "i686",
            Self::X86_64 => "x86_64",
            Self::Arm64 => "aarch64",
        }
    }

    /// The architecture this process is running as.
    ///
    /// A 32-bit process on a 64-bit Windows machine is a 32-bit machine for
    /// selection purposes, which is exactly the case a 32-bit dispatcher exists
    /// to handle: it must pick the x86 variant and run it natively.
    pub fn process() -> Self {
        Self::from_name(&zup_core::host_architecture()).unwrap_or(Self::X86_64)
    }

    /// The architecture this name denotes, if this build recognizes it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "x86" | "i386" | "i586" | "i686" => Some(Self::X86),
            "x86_64" | "amd64" => Some(Self::X86_64),
            "aarch64" | "arm64" => Some(Self::Arm64),
            _ => None,
        }
    }

    /// The architecture a triple names, if this build recognizes it.
    pub fn of_triple(triple: &TargetTriple) -> Option<Self> {
        Self::from_name(&triple.architecture().to_string())
    }
}

/// What this machine can run.
///
/// `native_execution` is the claim that matters: a Windows on ARM machine can
/// run an x86 variant through emulation, and a variant that installs drivers or
/// machine-wide components cannot. A bootstrapper that lies about this picks a
/// variant whose own install would be wrong on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    pub architecture: HostArchitecture,
    /// The operating system, as the canonical triple spelling: `windows`,
    /// `linux`, `macos`.
    pub os: String,
    /// Whether variants run natively, rather than through an emulation layer.
    pub native_execution: bool,
    /// Frontends this machine can present. Empty means "no opinion".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontends: Vec<String>,
}

impl Default for HostProfile {
    fn default() -> Self {
        Self {
            architecture: HostArchitecture::process(),
            os: zup_core::host_operating_system(),
            native_execution: true,
            frontends: Vec::new(),
        }
    }
}

impl HostProfile {
    /// A profile with no emulation available, which is the default.
    pub fn native() -> Self {
        Self::default()
    }

    /// The same machine, reachable only through an emulation layer.
    pub fn emulated(self) -> Self {
        Self {
            native_execution: false,
            ..self
        }
    }

    /// Declare that this machine can present `frontend`.
    pub fn with_frontend(mut self, frontend: impl Into<String>) -> Self {
        let frontend = frontend.into();
        if !self.frontends.iter().any(|known| known == &frontend) {
            self.frontends.push(frontend);
        }
        self
    }
}

/// Why a host cannot run a variant, or `None` when it can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incompatible {
    /// The variant that was refused.
    pub variant: String,
    /// The requirement it declares, in words a person can act on.
    pub requirement: String,
    /// What the host reported.
    pub found: String,
}

impl std::fmt::Display for Incompatible {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "variant `{}` needs {} (this machine is {})",
            self.variant, self.requirement, self.found
        )
    }
}

/// Whether `host` can run `variant`.
pub fn check_variant(host: &HostProfile, variant: &ReleaseVariant) -> Option<Incompatible> {
    let Some(architecture) = HostArchitecture::of_triple(&variant.target) else {
        return Some(Incompatible {
            variant: variant.id.clone(),
            requirement: "a recognized target architecture".to_owned(),
            found: variant.target.to_string(),
        });
    };
    if architecture != host.architecture {
        return Some(Incompatible {
            variant: variant.id.clone(),
            requirement: format!("architecture {}", architecture.as_str()),
            found: format!("architecture {}", host.architecture.as_str()),
        });
    }
    let os = variant.target.operating_system().to_string();
    if os != host.os {
        return Some(Incompatible {
            variant: variant.id.clone(),
            requirement: format!("operating system {os}"),
            found: format!("operating system {}", host.os),
        });
    }
    if variant.requirements.native_execution && !host.native_execution {
        return Some(Incompatible {
            variant: variant.id.clone(),
            requirement: "native execution".to_owned(),
            found: "an emulation layer".to_owned(),
        });
    }
    if !host.frontends.is_empty()
        && !host
            .frontends
            .iter()
            .any(|known| known == &variant.frontend)
    {
        return Some(Incompatible {
            variant: variant.id.clone(),
            requirement: format!("the {} frontend", variant.frontend),
            found: host.frontends.join(" or "),
        });
    }
    None
}

/// Choose the one variant this host installs.
///
/// The release names at most one variant per target, so compatibility is a
/// filter rather than a ranking: exactly one variant may match, and anything
/// else is a refusal. A ranked "best match" would make an install depend on a
/// scoring function nobody can audit from the release descriptor.
pub fn select_variant<'a>(
    release: &'a ReleaseDescriptor,
    host: &HostProfile,
) -> Result<&'a ReleaseVariant, SelectionError> {
    let mut matches: Vec<&ReleaseVariant> = Vec::new();
    let mut refusals = Vec::new();
    for variant in &release.variants {
        match check_variant(host, variant) {
            None => matches.push(variant),
            Some(reason) => refusals.push(reason),
        }
    }
    let detail = |reasons: &[Incompatible]| {
        reasons
            .iter()
            .map(|refusal| refusal.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    };
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(SelectionError::NoCompatibleVariant {
            app_id: release.app_id.to_string(),
            version: release.version.clone(),
            detail: detail(&refusals),
        }),
        _ => Err(SelectionError::Ambiguous {
            app_id: release.app_id.to_string(),
            version: release.version.clone(),
            detail: matches
                .iter()
                .map(|variant| format!("`{}`", variant.id))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Why a host could not be given exactly one variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("no variant in {app_id} {version} can run here: {detail}")]
    NoCompatibleVariant {
        app_id: String,
        version: String,
        detail: String,
    },
    #[error("more than one variant in {app_id} {version} claims this machine: {detail}")]
    Ambiguous {
        app_id: String,
        version: String,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::{DocumentRef, RELEASE_SCHEMA, ReleaseRequirements};
    use zup_core::Sha256Digest;

    fn triple(architecture: &str) -> TargetTriple {
        TargetTriple::parse(format!("{architecture}-pc-windows-msvc")).expect("valid triple")
    }

    fn variant(id: &str, target: TargetTriple, native: bool) -> ReleaseVariant {
        ReleaseVariant {
            id: id.to_owned(),
            target,
            platform: "windows".to_owned(),
            frontend: "gui".to_owned(),
            manifest: DocumentRef::of(Sha256Digest::from_bytes([1; 32]), 10),
            runtime: None,
            content: vec![Sha256Digest::from_bytes([2; 32])],
            requirements: ReleaseRequirements {
                native_execution: native,
                capabilities: Vec::new(),
            },
            logical_size: 10,
        }
    }

    fn release(variants: Vec<ReleaseVariant>) -> ReleaseDescriptor {
        let mut release = ReleaseDescriptor {
            schema: RELEASE_SCHEMA,
            app_id: zup_core::AppId::new("com.example.app").expect("valid app id"),
            channel: "stable".to_owned(),
            version: "1.4.0".to_owned(),
            release_digest: Sha256Digest::from_bytes([0; 32]),
            catalog: DocumentRef::of(Sha256Digest::from_bytes([3; 32]), 10),
            variants,
            downloads: Vec::new(),
        };
        release.release_digest = release.computed_digest().expect("fingerprints");
        release
    }

    fn windows_x64() -> HostProfile {
        HostProfile {
            architecture: HostArchitecture::X86_64,
            os: "windows".to_owned(),
            native_execution: true,
            frontends: Vec::new(),
        }
    }

    #[test]
    fn an_x86_host_never_picks_the_x64_variant() {
        let release = release(vec![
            variant("x64", triple("x86_64"), false),
            variant("x86", triple("i686"), false),
        ]);
        let host = HostProfile {
            architecture: HostArchitecture::X86,
            ..windows_x64()
        };
        assert_eq!(select_variant(&release, &host).expect("selects").id, "x86");
    }

    #[test]
    fn a_variant_that_needs_native_execution_is_refused_under_emulation() {
        let release = release(vec![variant("x64", triple("x86_64"), true)]);
        let error = select_variant(&release, &windows_x64().emulated())
            .expect_err("emulation is not enough");
        assert!(
            matches!(error, SelectionError::NoCompatibleVariant { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("native execution"));
    }

    #[test]
    fn a_refusal_names_the_architecture_it_looked_for() {
        let release = release(vec![variant("x64", triple("x86_64"), true)]);
        let host = HostProfile {
            architecture: HostArchitecture::Arm64,
            ..windows_x64()
        };
        let error = select_variant(&release, &host).expect_err("nothing matches");
        assert!(error.to_string().contains("aarch64"), "{error}");
    }

    #[test]
    fn the_selection_is_a_filter_not_a_ranking() {
        // Two variants that both claim the machine is a defect in the release,
        // and the refusal says so rather than silently picking one.
        let release = release(vec![
            variant("a", triple("x86_64"), false),
            variant("b", triple("x86_64"), false),
        ]);
        let error = select_variant(&release, &windows_x64()).expect_err("ambiguous");
        assert!(
            matches!(error, SelectionError::Ambiguous { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("`a`"), "{error}");
        assert!(error.to_string().contains("`b`"), "{error}");
    }

    #[test]
    fn a_frontend_the_host_cannot_present_is_a_refusal() {
        let release = release(vec![variant("x64", triple("x86_64"), false)]);
        let host = windows_x64().with_frontend("console");
        let error = select_variant(&release, &host).expect_err("the host is console only");
        assert!(error.to_string().contains("gui"), "{error}");
        assert!(select_variant(&release, &windows_x64().with_frontend("gui")).is_ok());
    }
}
