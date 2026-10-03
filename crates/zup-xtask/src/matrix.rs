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

/// Whose vocabulary a matrix's packages are allowed to use.
///
/// The portable boundary forbids a portable package from presenting a Windows
/// concept as part of the domain model, because a model that names `HKEY_LOCAL_MACHINE`
/// is a model that only one platform can be right about. That argument does not
/// reach a crate whose *domain* is a Windows file format: a Portable Executable
/// has the same bytes on every host, and `RCDATA` in a PE parser is the format's
/// own constant, exactly as `TargetOperatingSystem::Windows` is a target
/// lexicon's own constant in a portable crate today.
///
/// The relaxation is to the vocabulary rules only. A file-format crate still may
/// not depend on a Windows crate, branch on the build host, import
/// `std::os::windows`, or name a Win32 namespace: the format is portable, so its
/// implementation has to be too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vocabulary {
    /// The package's own domain vocabulary, which must not name a Windows concept.
    Domain,
    /// A platform file format, whose vocabulary is the format's.
    FileFormat,
}

/// One named package set.
#[derive(Debug, Clone, Copy)]
pub struct Matrix {
    pub name: &'static str,
    /// Shown next to the name in the emitted text view.
    pub summary: &'static str,
    pub host: Host,
    pub vocabulary: Vocabulary,
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
    // The published UI contract. Portable because a preset outside this repository
    // compiles it, and a contract only one platform can compile is not one they can
    // consume. It is deliberately free of GPUI, Windows APIs and Tokio, so nothing
    // about it is platform-shaped.
    "zup-preset-protocol",
    // The transport a preset and its host speak over. Portable for the same
    // reason the contract is: it is what makes a preset source portable, and a
    // transport only one platform can compile is not a portable one. It is
    // portable by using the operating system's own IPC rather than by
    // reimplementing any of it.
    "zup-preset-ipc",
    // The host side of the UI protocol, with no engine behind it. Portable because a
    // preset author meets it in zup ui dev before they meet an installer, and a
    // state machine only one platform can run is not one they can develop against.
    "zup-preset-host",
    // Which window an application presents, and what it is given. Portable because
    // a Linux CI job that checks a project does not become unable to answer the
    // question, and a rule only one platform can apply is a rule only one platform
    // is checking.
    "zup-preset-compose",
    // The machine a preset is previewed against, with no engine behind it.
    // Portable because it spawns processes, watches a directory and drives a
    // state machine, none of which is a platform, and a preview environment only
    // one platform can run is an environment nobody outside that platform can
    // develop a portable preset with.
    "zup-preview",
    // The source half of `zup ui dev`: the Cargo project a preset author edits
    // and the compiler that has to run for it. Portable for the same reason.
    "zup-preset-dev",
    "zup-update",
    "zup-plugin-contract",
    "zup-plugin-build",
    "zup-plugin-runtime",
    // The toolchain compatibility contract. Portable because the repository's own
    // tooling writes these descriptors on a host that cannot run - or even
    // compile - the components they describe.
    "zup-toolchain",
    // The signing, evidence and finalization contract. Portable because it is a
    // description of a release, and a description of a release is the same on
    // every platform. It knows nothing about Authenticode, `codesign`, a
    // certificate store or a TSA; those are integrations.
    "zup-signing",
    // The developer CLI's machine contract. Portable because a CI system on Linux, a
    // generated TypeScript declaration and a Windows build all have to agree about the
    // same bytes, and a contract only one of them can compile is not a contract.
    "zup-automation",
];

/// Crates whose domain is a platform file format.
///
/// Separate from [`PORTABLE_CORE`] because of vocabulary, not portability: a PE
/// image, a resource table and an Authenticode digest are facts about bytes, and
/// reading them is the same work on every host. What they do not include is
/// WinVerifyTrust, `UpdateResourceW`, or any other host's judgement about a file:
/// those are the Windows adapter's, and a file-format crate that grew one would
/// stop being portable in the way that matters.
///
/// `zup-binary` is here for the same reason and covers PE/COFF, ELF and Mach-O
/// at once. It has no Windows vocabulary of its own to relax, but it is a
/// platform file format crate by the same argument, and classifying it here keeps
/// that a fact about what it is rather than a judgement made per call site.
/// `zup-assets` is here because ICO and ICNS are file formats: it names them,
/// and it still has no host API.
pub const PORTABLE_FILE_FORMAT: &[&str] = &["zup-pe", "zup-binary", "zup-assets"];

/// Portable crates that verify the stack instead of shipping inside an
/// installer. Their test suites are part of the native portable run.
pub const PORTABLE_TESTS: &[&str] = &["zup-xtask"];

/// Portable packages whose `cfg` branch selects between two spellings of one
/// behaviour that a portable dependency already provides per platform.
///
/// The [`Vocabulary`] relaxation is about *words*: a file-format crate must name
/// the format it reads. This one is about a *branch*, and it is much narrower.
/// `process-wrap` exposes exactly one process-tree mechanism per platform - a job
/// object on Windows, a process group on Unix - and no portable spelling of "the
/// tree is mine". A host that owns a child has to name which one it is using, and
/// the two names compile to the same guarantee on both platforms.
///
/// What is still refused for these packages, unchanged: a Windows dependency, a
/// Windows-only manifest target, `std::os::windows`, and a Win32 namespace. The
/// branch may only choose between the two wrappers `process-wrap` documents, and a
/// package gains this by appearing here rather than by a line added beside the
/// code it silences.
pub const PORTABLE_PLATFORM_DELEGATING: &[&str] = &["zup-preset-host", "zup-preset-dev"];

/// Crates that require a Windows build host: the Windows adapter, the
/// composition CLI, the runtime an installer embeds, the native frontends, and
/// the small dispatcher a universal artifact starts through.
pub const WINDOWS_ONLY: &[&str] = &[
    "zup-windows",
    "zup-dispatch",
    "zup",
    "zup-installer",
    // The default installer interface, which is a GPUI application like any
    // other preset and is built by the toolchain rather than shipped inside an
    // installer as a link-time dependency.
    //
    // The SDK is here rather than in the portable core because its platform is
    // GPUI's, not Zup's: a preset author writes the same source on every host
    // and the GPUI stack decides which of them it builds for. The transport
    // underneath it is portable, and is `zup-preset-ipc`.
    "zup-preset-sdk",
    // The two presets this repository builds. `zup-preset-default` is the one a
    // graphical install presents unless an application names another, and
    // `zup-preset-test` is the child the installer's end-to-end test launches;
    // they are the same shape written twice, which is the point of keeping the
    // second one a separate package rather than a mode of the first.
    "zup-preset-default",
    "zup-preset-test",
];

pub const MATRICES: &[Matrix] = &[
    Matrix {
        name: "portable-core",
        summary: "crates that build and test on a non-Windows host",
        host: Host::Any,
        vocabulary: Vocabulary::Domain,
        packages: PORTABLE_CORE,
    },
    Matrix {
        name: "portable-file-format",
        summary: "crates whose domain is a platform file format, read the same way on every host",
        host: Host::Any,
        vocabulary: Vocabulary::FileFormat,
        packages: PORTABLE_FILE_FORMAT,
    },
    Matrix {
        name: "portable-tests",
        summary: "portable crates that verify the stack instead of shipping in an installer",
        host: Host::Any,
        vocabulary: Vocabulary::Domain,
        packages: PORTABLE_TESTS,
    },
    Matrix {
        name: "windows-only",
        summary: "crates that require a Windows build host",
        host: Host::Windows,
        vocabulary: Vocabulary::Domain,
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

/// The vocabulary a portable package is held to, and `None` for a package no
/// matrix claims. Callers treat a package that is not in a matrix as
/// [`Vocabulary::Domain`], so an unclassified crate gets the strictest rules
/// rather than none.
pub fn vocabulary_of(package: &str) -> Vocabulary {
    MATRICES
        .iter()
        .find(|matrix| matrix.packages.contains(&package))
        .map_or(Vocabulary::Domain, |matrix| matrix.vocabulary)
}

/// Whether a portable package may branch on the build host to choose between two
/// spellings of one portable dependency's per-platform API.
///
/// Declared by list rather than by an allowlist next to the caller, so a package
/// does not gain the relaxation by a line added beside the code it silences, and
/// so the set is reviewable in one place. An unclassified package gets the strict
/// answer.
pub fn delegates_platform_lifecycle(package: &str) -> bool {
    PORTABLE_PLATFORM_DELEGATING.contains(&package)
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
