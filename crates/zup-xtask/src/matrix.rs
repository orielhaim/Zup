use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Any,
    Windows,
    Linux,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    Windows,
    Linux,
}

impl Platform {
    pub const fn crate_name(self) -> &'static str {
        match self {
            Self::Windows => "zup-windows",
            Self::Linux => "zup-linux",
        }
    }

    pub const ALL: &'static [Platform] = &[Platform::Windows, Platform::Linux];

    pub const fn host(self) -> Host {
        match self {
            Self::Windows => Host::Windows,
            Self::Linux => Host::Linux,
        }
    }

    pub const fn on(host: Host) -> &'static [Platform] {
        match host {
            Host::Any => &[],
            Host::Windows => &[Platform::Windows],
            Host::Linux => &[Platform::Linux],
            Host::Both => &[Platform::Windows, Platform::Linux],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Portable,
    Backend(Platform),
    Composition,
}

impl Kind {
    pub const fn is_portable(self) -> bool {
        matches!(self, Self::Portable)
    }

    pub const fn is_composition(self) -> bool {
        matches!(self, Self::Composition)
    }

    pub const fn backend(self) -> Option<Platform> {
        match self {
            Self::Backend(platform) => Some(platform),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vocabulary {
    Domain,
    FileFormat,
}

#[derive(Debug, Clone, Copy)]
pub struct Matrix {
    pub name: &'static str,
    pub summary: &'static str,
    pub host: Host,
    pub kind: Kind,
    pub vocabulary: Vocabulary,
    pub packages: &'static [&'static str],
}

pub const PORTABLE_CORE: &[&str] = &[
    "zup-core",
    "zup-manifest",
    "zup-build",
    "zup-plan",
    "zup-platform",
    "zup-exec",
    "zup-transaction",
    "zup-bootstrap",
    "zup-bundle",
    "zup-acquire",
    "zup-acquire-http",
    "zup-artifact",
    "zup-publish",
    "zup-publish-github",
    "zup-distribute-github",
    "zup-protocol",
    "zup-runtime",
    "zup-presentation",
    "zup-preset-protocol",
    "zup-preset-ipc",
    "zup-plugin-abi",
    "zup-sdk-macros",
    "zup-preset-host",
    "zup-preset-compose",
    "zup-preview",
    "zup-preset-dev",
    "zup-update",
    "zup-plugin-contract",
    "zup-plugin-build",
    "zup-plugin-runtime",
    "zup-toolchain",
    "zup-signing",
    "zup-automation",
];

pub const PORTABLE_FILE_FORMAT: &[&str] = &["zup-pe", "zup-binary", "zup-assets"];

pub const PORTABLE_TESTS: &[&str] = &["zup-xtask"];

pub const PORTABLE_PLATFORM_DELEGATING: &[&str] = &["zup-preset-host", "zup-preset-dev"];

pub const PORTABLE_POSIX_PERMISSIONS: &[&str] = &["zup-preview"];

pub const WINDOWS_BACKEND: &[&str] = &["zup-windows"];

pub const LINUX_BACKEND: &[&str] = &["zup-linux"];

pub const NATIVE_BACKENDS: &[&str] = &["zup-windows", "zup-linux"];

pub const WINDOWS_COMPOSITION: &[&str] = &[
    "zup-dispatch",
    "zup-sdk",
    "zup-preset-default",
    "zup-preset-test",
];

pub const MULTI_PLATFORM_COMPOSITION: &[&str] = &["zup", "zup-installer"];

pub const MATRICES: &[Matrix] = &[
    Matrix {
        name: "portable-core",
        summary: "crates that build and test on a host with no native backend",
        host: Host::Any,
        kind: Kind::Portable,
        vocabulary: Vocabulary::Domain,
        packages: PORTABLE_CORE,
    },
    Matrix {
        name: "portable-file-format",
        summary: "crates whose domain is a platform file format, read the same way on every host",
        host: Host::Any,
        kind: Kind::Portable,
        vocabulary: Vocabulary::FileFormat,
        packages: PORTABLE_FILE_FORMAT,
    },
    Matrix {
        name: "portable-tests",
        summary: "portable crates that verify the stack instead of shipping in an installer",
        host: Host::Any,
        kind: Kind::Portable,
        vocabulary: Vocabulary::Domain,
        packages: PORTABLE_TESTS,
    },
    Matrix {
        name: "windows-backend",
        summary: "the Windows mechanism layer",
        host: Host::Windows,
        kind: Kind::Backend(Platform::Windows),
        vocabulary: Vocabulary::Domain,
        packages: WINDOWS_BACKEND,
    },
    Matrix {
        name: "linux-backend",
        summary: "the Linux mechanism layer",
        host: Host::Linux,
        kind: Kind::Backend(Platform::Linux),
        vocabulary: Vocabulary::Domain,
        packages: LINUX_BACKEND,
    },
    Matrix {
        name: "windows-composition",
        summary: "composition and tooling that is built on the Windows backend",
        host: Host::Windows,
        kind: Kind::Composition,
        vocabulary: Vocabulary::Domain,
        packages: WINDOWS_COMPOSITION,
    },
    Matrix {
        name: "multi-platform-composition",
        summary: "composition that builds on the Windows and Linux backends alike",
        host: Host::Both,
        kind: Kind::Composition,
        vocabulary: Vocabulary::Domain,
        packages: MULTI_PLATFORM_COMPOSITION,
    },
];

pub fn names() -> Vec<&'static str> {
    MATRICES.iter().map(|matrix| matrix.name).collect()
}

pub fn matrix(name: &str) -> Option<&'static Matrix> {
    MATRICES.iter().find(|matrix| matrix.name == name)
}

pub fn is_portable(package: &str) -> bool {
    kind_of(package).is_some_and(Kind::is_portable)
}

pub fn kind_of(package: &str) -> Option<Kind> {
    MATRICES
        .iter()
        .find(|matrix| matrix.packages.contains(&package))
        .map(|matrix| matrix.kind)
}

pub fn is_composition(package: &str) -> bool {
    kind_of(package).is_some_and(Kind::is_composition)
}

pub fn is_backend(package: &str) -> bool {
    kind_of(package).is_some_and(|kind| kind.backend().is_some())
}

pub fn packages_for_host(host: Host) -> Vec<&'static str> {
    MATRICES
        .iter()
        .filter(|matrix| matrix.host == host || (matrix.host == Host::Both && host != Host::Any))
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

pub fn host_name(host: Host) -> &'static str {
    match host {
        Host::Any => "any",
        Host::Windows => "windows",
        Host::Linux => "linux",
        Host::Both => "windows+linux",
    }
}

pub fn all() -> Vec<&'static str> {
    MATRICES
        .iter()
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

pub fn portable() -> Vec<&'static str> {
    MATRICES
        .iter()
        .filter(|matrix| matrix.kind.is_portable())
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

pub fn backends() -> Vec<&'static str> {
    Platform::ALL
        .iter()
        .map(|platform| platform.crate_name())
        .collect()
}

pub fn sibling_backends(platform: Platform) -> Vec<&'static str> {
    Platform::ALL
        .iter()
        .filter(|other| **other != platform)
        .map(|other| other.crate_name())
        .collect()
}

pub fn vocabulary_of(package: &str) -> Vocabulary {
    MATRICES
        .iter()
        .find(|matrix| matrix.packages.contains(&package))
        .map_or(Vocabulary::Domain, |matrix| matrix.vocabulary)
}

pub fn delegates_platform_lifecycle(package: &str) -> bool {
    PORTABLE_PLATFORM_DELEGATING.contains(&package)
}

pub fn writes_posix_permissions(package: &str) -> bool {
    PORTABLE_POSIX_PERMISSIONS.contains(&package)
}

pub fn duplicated_packages() -> Vec<&'static str> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for package in all() {
        *counts.entry(package).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(package, _)| package)
        .collect()
}

pub fn render(matrices: &[&'static Matrix]) -> String {
    let mut out = String::new();
    for matrix in matrices {
        let _ = writeln!(out, "{}: {}", matrix.name, matrix.summary);
        for package in matrix.packages {
            let _ = writeln!(out, "  {package}");
        }
    }
    out
}

pub fn render_cargo_args(matrix: &Matrix) -> String {
    matrix
        .packages
        .iter()
        .map(|package| format!("-p {package}"))
        .collect::<Vec<_>>()
        .join(" ")
}
