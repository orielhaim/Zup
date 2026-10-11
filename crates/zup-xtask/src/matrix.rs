use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Any,
    Windows,
    Linux,
    Both,
}

#[derive(Debug, Clone, Copy)]
pub struct Matrix {
    pub name: &'static str,
    pub summary: &'static str,
    pub host: Host,
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

pub const WINDOWS_BACKEND: &[&str] = &["zup-windows"];

pub const LINUX_BACKEND: &[&str] = &["zup-linux"];

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
        packages: PORTABLE_CORE,
    },
    Matrix {
        name: "portable-file-format",
        summary: "crates whose domain is a platform file format",
        host: Host::Any,
        packages: PORTABLE_FILE_FORMAT,
    },
    Matrix {
        name: "portable-tests",
        summary: "portable crates that verify the stack instead of shipping in an installer",
        host: Host::Any,
        packages: PORTABLE_TESTS,
    },
    Matrix {
        name: "windows-backend",
        summary: "the Windows backend: portable composition on any host, native mechanisms on Windows",
        host: Host::Both,
        packages: WINDOWS_BACKEND,
    },
    Matrix {
        name: "linux-backend",
        summary: "the Linux backend: portable composition on any host, native mechanisms on Linux",
        host: Host::Both,
        packages: LINUX_BACKEND,
    },
    Matrix {
        name: "windows-composition",
        summary: "composition and tooling that is built on the Windows backend",
        host: Host::Windows,
        packages: WINDOWS_COMPOSITION,
    },
    Matrix {
        name: "multi-platform-composition",
        summary: "composition that builds on the Windows and Linux backends alike",
        host: Host::Both,
        packages: MULTI_PLATFORM_COMPOSITION,
    },
];

pub fn names() -> Vec<&'static str> {
    MATRICES.iter().map(|matrix| matrix.name).collect()
}

pub fn matrix(name: &str) -> Option<&'static Matrix> {
    MATRICES.iter().find(|matrix| matrix.name == name)
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
