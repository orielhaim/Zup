//! The authoritative package matrices.
//!
//! This module is the only place a package is classified. Every command, test
//! fixture, documentation example, and CI job derives its package list from
//! here, so a new crate is portable or host-specific by construction.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Which build hosts a matrix is verified on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// Compiles and tests on every host, including non-Windows hosts.
    Any,
    /// Requires a Windows build host.
    Windows,
}

/// One named package set.
#[derive(Debug, Clone, Copy)]
pub struct Matrix {
    pub name: &'static str,
    /// Shown next to the name in the emitted text view.
    pub summary: &'static str,
    pub host: Host,
    /// Declaration order, preserved in every emitted view.
    pub packages: &'static [&'static str],
}

/// Crates whose production code and tests must build and run on a non-Windows
/// host: the semantic model, the build inventory, planning, execution,
/// transaction, packaging, runtime, presentation, updates, plugins, and the
/// release plane.
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
    "zup-update",
    "zup-plugin-contract",
    "zup-plugin-build",
    "zup-plugin-runtime",
    // The toolchain compatibility contract. Portable because the repository's own
    // tooling writes these descriptors on a host that cannot run — or even
    // compile — the components they describe.
    "zup-toolchain",
];

/// Portable crates that verify the stack instead of shipping inside an
/// installer. Their test suites are part of the native portable run.
pub const PORTABLE_TESTS: &[&str] = &["zup-xtask"];

/// Crates that require a Windows build host: the Windows adapter, the
/// composition CLI, the runtime an installer embeds, the native frontends, and
/// the small dispatcher a universal artifact starts through.
pub const WINDOWS_ONLY: &[&str] = &[
    "zup-pe",
    "zup-windows",
    "zup-dispatch",
    "zup",
    "zup-installer",
    "zup-ui",
];

pub const MATRICES: &[Matrix] = &[
    Matrix {
        name: "portable-core",
        summary: "crates that build and test on a non-Windows host",
        host: Host::Any,
        packages: PORTABLE_CORE,
    },
    Matrix {
        name: "portable-tests",
        summary: "portable crates that verify the stack instead of shipping in an installer",
        host: Host::Any,
        packages: PORTABLE_TESTS,
    },
    Matrix {
        name: "windows-only",
        summary: "crates that require a Windows build host",
        host: Host::Windows,
        packages: WINDOWS_ONLY,
    },
];

/// Every matrix name, in declaration order.
pub fn names() -> Vec<&'static str> {
    MATRICES.iter().map(|matrix| matrix.name).collect()
}

/// Look up one matrix by name.
pub fn matrix(name: &str) -> Option<&'static Matrix> {
    MATRICES.iter().find(|matrix| matrix.name == name)
}

/// Whether `package` is verified on any build host.
pub fn is_portable(package: &str) -> bool {
    MATRICES
        .iter()
        .filter(|matrix| matrix.host == Host::Any)
        .any(|matrix| matrix.packages.contains(&package))
}

/// Every package, in matrix order and then declaration order.
pub fn all() -> Vec<&'static str> {
    MATRICES
        .iter()
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

/// Every portable package, in matrix order and then declaration order.
pub fn portable() -> Vec<&'static str> {
    MATRICES
        .iter()
        .filter(|matrix| matrix.host == Host::Any)
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

/// Packages listed in more than one matrix. A package belongs to exactly one.
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

/// The text view: a summary line per matrix, then one indented package per line.
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

/// Cargo `-p` arguments for one matrix, ready to splat into a cargo command.
pub fn render_cargo_args(matrix: &Matrix) -> String {
    matrix
        .packages
        .iter()
        .map(|package| format!("-p {package}"))
        .collect::<Vec<_>>()
        .join(" ")
}
