use crate::compat::{requires_native, satisfies_minimum_host};
use crate::format::ArtifactError;
use crate::index::ArtifactIndex;
use crate::platform::{HostArchitecture, Platform};
use crate::variant::{HostVersion, PlatformOs, VariantDescriptor, VariantRequirements};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Compatibility {
    Native,
    Emulated,
    Unsupported,
}

impl Compatibility {
    const fn rank(self) -> u8 {
        match self {
            Self::Native => 2,
            Self::Emulated => 1,
            Self::Unsupported => 0,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Emulated => "emulated",
            Self::Unsupported => "unsupported",
        }
    }
}

impl std::fmt::Display for Compatibility {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// never asks *why* a host can execute a foreign architecture, only that it can,
/// and never asks which mechanism provides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExecution {
    pub os: PlatformOs,
    pub native: HostArchitecture,
    pub emulated: Vec<HostArchitecture>,
    pub version: Option<HostVersion>,
}

impl HostExecution {
    pub fn native_only(os: PlatformOs, native: HostArchitecture) -> Self {
        Self {
            os,
            native,
            emulated: Vec::new(),
            version: None,
        }
    }

    pub fn compatibility(&self, platform: &Platform) -> Compatibility {
        if PlatformOs::from_name(&platform.os) != self.os {
            return Compatibility::Unsupported;
        }
        let Some(architecture) = platform.host_architecture() else {
            return Compatibility::Unsupported;
        };
        if architecture == self.native {
            return Compatibility::Native;
        }
        if self.emulated.contains(&architecture) {
            return Compatibility::Emulated;
        }
        Compatibility::Unsupported
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateVariant<'a> {
    pub id: &'a str,
    pub platform: &'a Platform,
    pub requirements: &'a VariantRequirements,
    pub runtime: Option<&'a zup_core::Sha256Digest>,
}

impl<'a> CandidateVariant<'a> {
    pub fn of(variant: &'a VariantDescriptor) -> Self {
        Self {
            id: variant.id.as_str(),
            platform: &variant.platform,
            requirements: &variant.requirements,
            runtime: variant.runtime.as_ref().map(|runtime| &runtime.digest),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredCandidate<'a> {
    pub candidate: CandidateVariant<'a>,
    pub compatibility: Compatibility,
    pub width: u8,
}

impl<'a> ScoredCandidate<'a> {
    pub fn is_selectable(&self) -> bool {
        match self.compatibility {
            Compatibility::Unsupported => false,
            Compatibility::Native => true,
            Compatibility::Emulated => {
                self.candidate.requirements.permits_emulation()
                    && !requires_native(&self.candidate.requirements.capabilities)
            }
        }
    }

    pub fn satisfies_host(&self, host: &HostExecution) -> bool {
        satisfies_minimum_host(
            self.candidate.requirements.minimum_host.as_ref(),
            Some(host.os),
            host.version.as_ref(),
        )
    }
}

pub fn score<'a>(host: &HostExecution, candidate: CandidateVariant<'a>) -> ScoredCandidate<'a> {
    ScoredCandidate {
        compatibility: host.compatibility(candidate.platform),
        width: candidate
            .platform
            .host_architecture()
            .map_or(0, HostArchitecture::width),
        candidate,
    }
}

pub fn rank_all<'a>(
    host: &HostExecution,
    candidates: &[CandidateVariant<'a>],
) -> Vec<ScoredCandidate<'a>> {
    candidates
        .iter()
        .map(|candidate| score(host, *candidate))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<'a> {
    pub candidate: CandidateVariant<'a>,
    pub compatibility: Compatibility,
}

pub fn select<'a>(
    host: &HostExecution,
    candidates: &[CandidateVariant<'a>],
) -> Result<Selection<'a>, ArtifactError> {
    select_in(host, candidates, None)
}

pub fn select_in<'a>(
    host: &HostExecution,
    candidates: &[CandidateVariant<'a>],
    artifact: Option<&str>,
) -> Result<Selection<'a>, ArtifactError> {
    let scored = rank_all(host, candidates);
    let selectable = |candidate: &ScoredCandidate<'a>| {
        candidate.is_selectable() && candidate.satisfies_host(host)
    };
    let best = scored
        .iter()
        .filter(|candidate| selectable(candidate))
        .map(|candidate| (candidate.compatibility.rank(), candidate.width))
        .max();
    let Some((rank, width)) = best else {
        return Err(ArtifactError::NoCompatibleVariant {
            id: artifact.unwrap_or("artifact").to_owned(),
            detail: refusal_detail(host, candidates),
        });
    };
    let winner = scored
        .iter()
        .filter(|candidate| selectable(candidate))
        .find(|candidate| candidate.compatibility.rank() == rank && candidate.width == width)
        .expect("the best rank came from this collection");
    let mut tied: Vec<&str> = scored
        .iter()
        .filter(|candidate| selectable(candidate))
        .filter(|candidate| candidate.compatibility.rank() == rank && candidate.width == width)
        .map(|candidate| candidate.candidate.id)
        .collect();
    tied.sort_unstable();
    if tied.len() > 1 {
        return Err(ArtifactError::Ambiguous {
            left: tied[0].to_owned(),
            right: tied[1].to_owned(),
        });
    }
    Ok(Selection {
        candidate: winner.candidate,
        compatibility: winner.compatibility,
    })
}

fn refusal_detail(host: &HostExecution, candidates: &[CandidateVariant<'_>]) -> String {
    let mut detail = format!(
        "host is {} {}{}",
        host.os.as_str(),
        host.native,
        match host.emulated.len() {
            0 => String::new(),
            count => format!(" (and executes {} other architectures)", count),
        }
    );
    let same_os: Vec<&CandidateVariant<'_>> = candidates
        .iter()
        .filter(|candidate| PlatformOs::from_name(&candidate.platform.os) == host.os)
        .collect();
    if same_os.is_empty() {
        detail.push_str("; no variant targets this operating system");
        return detail;
    }
    let has_native = same_os
        .iter()
        .any(|candidate| candidate.platform.host_architecture() == Some(host.native));
    if has_native {
        detail.push_str(
            "; every variant for this machine requires native execution or a newer host than this one has",
        );
    } else {
        detail.push_str("; the artifact has no variant built for this machine architecture");
    }
    detail
}

pub fn select_from_index<'a>(
    host: &HostExecution,
    index: &'a ArtifactIndex,
) -> Result<Selection<'a>, ArtifactError> {
    let candidates = index
        .variants
        .iter()
        .map(CandidateVariant::of)
        .collect::<Vec<_>>();
    select_in(host, &candidates, Some(&index.artifact.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variant::{MinimumHost, PlatformCapability};

    fn candidate(
        id: &'static str,
        architecture: &'static str,
        requirements: VariantRequirements,
    ) -> CandidateVariant<'static> {
        CandidateVariant {
            id,
            platform: Box::leak(Box::new(Platform {
                os: "windows".into(),
                architecture: architecture.into(),
                vendor: Some("pc".into()),
                abi: Some("msvc".into()),
                variant: None,
            })),
            requirements: Box::leak(Box::new(requirements)),
            runtime: None,
        }
    }

    fn x64() -> CandidateVariant<'static> {
        candidate("x64", "x86_64", VariantRequirements::default())
    }

    fn arm64_native_only() -> CandidateVariant<'static> {
        candidate(
            "arm64-native",
            "aarch64",
            VariantRequirements {
                native_execution: true,
                capabilities: Vec::new(),
                minimum_host: None,
            },
        )
    }

    #[test]
    fn native_always_beats_a_supported_emulated_fallback() {
        let host = HostExecution {
            os: PlatformOs::Windows,
            native: HostArchitecture::Arm64,
            emulated: vec![HostArchitecture::X86_64],
            version: Some(HostVersion::new(11, 0, 0)),
        };
        let selection = select(&host, &[x64(), arm64_native_only()]).unwrap();
        assert_eq!(selection.candidate.id, "arm64-native");
        assert_eq!(selection.compatibility, Compatibility::Native);
    }

    #[test]
    fn a_variant_that_cannot_run_emulated_is_refused() {
        let host = HostExecution {
            os: PlatformOs::Windows,
            native: HostArchitecture::X86_64,
            emulated: vec![HostArchitecture::Arm64],
            version: Some(HostVersion::new(11, 0, 0)),
        };
        for (id, native_execution) in [("arm64-driver", false), ("arm64-strict", true)] {
            let variant = candidate(
                id,
                "aarch64",
                VariantRequirements {
                    native_execution,
                    capabilities: if native_execution {
                        Vec::new()
                    } else {
                        vec![PlatformCapability::MachineComponents]
                    },
                    minimum_host: None,
                },
            );
            assert!(
                !variant.requirements.permits_emulation(),
                "{id} must not permit emulation"
            );
            let error = select(&host, &[variant]).unwrap_err();
            assert!(error.is_unsupported_host(), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("no variant built for this machine architecture"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_minimum_host_requirement_is_enforced() {
        let host = HostExecution {
            os: PlatformOs::Windows,
            native: HostArchitecture::X86_64,
            emulated: Vec::new(),
            version: Some(HostVersion::new(10, 0, 17763)),
        };
        let minimum = candidate(
            "x64-modern",
            "x86_64",
            VariantRequirements {
                native_execution: false,
                capabilities: Vec::new(),
                minimum_host: Some(MinimumHost {
                    os: PlatformOs::Windows,
                    version: HostVersion::new(10, 0, 22000),
                }),
            },
        );
        assert!(select(&host, &[minimum]).unwrap_err().is_unsupported_host());
        let modern = HostExecution {
            version: Some(HostVersion::new(11, 0, 0)),
            ..host
        };
        assert_eq!(
            select(&modern, &[minimum]).unwrap().candidate.id,
            "x64-modern"
        );
    }

    #[test]
    fn an_unknown_host_version_refuses_a_variant_with_a_minimum() {
        let host = HostExecution::native_only(PlatformOs::Windows, HostArchitecture::X86_64);
        let minimum = candidate(
            "x64-modern",
            "x86_64",
            VariantRequirements {
                native_execution: false,
                capabilities: Vec::new(),
                minimum_host: Some(MinimumHost {
                    os: PlatformOs::Windows,
                    version: HostVersion::new(10, 0, 0),
                }),
            },
        );
        assert!(select(&host, &[minimum]).is_err());
    }
}
