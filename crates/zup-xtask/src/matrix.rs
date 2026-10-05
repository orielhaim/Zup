//! The authoritative package matrices.
//!
//! This module is the only place a package is classified. Every command, test
//! fixture, documentation example, and CI job derives its package list from
//! here, so a new crate is portable, a native backend, or composition by
//! construction.
//!
//! # Two questions, not one
//!
//! [`Host`] says which machines verify a package. [`Kind`] says what it is
//! allowed to reach. They are separate because a composition crate - the
//! developer CLI, an installer runtime - is verified on a native host *and*
//! reaches a backend deliberately, so a single axis would force it to be called
//! portable or called Windows, and it is neither.
//!
//! A third platform adds a [`Platform`] variant, a backend matrix, and its own
//! entry in [`Platform::ALL`]. Every rule that reads this module picks the new
//! platform up from there, so the boundary does not grow a one-off exception per
//! operating system.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Which build hosts a matrix is verified on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// Compiles and tests on every host, including hosts with no native backend.
    Any,
    /// Requires a Windows build host.
    Windows,
    /// Requires a Linux build host.
    Linux,
}

/// A native platform backend.
///
/// A backend owns one operating system's mechanisms and is named for it. Backends
/// are siblings: they share the portable crates beneath them and nothing with
/// each other, and that independence is the property that makes a second one
/// cheap to add and a third one possible.
///
/// A new operating system becomes a variant here and a new matrix below, rather
/// than another branch in every rule that reads this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    Windows,
    Linux,
}

impl Platform {
    /// The backend package that owns this platform's mechanisms.
    pub const fn crate_name(self) -> &'static str {
        match self {
            Self::Windows => "zup-windows",
            Self::Linux => "zup-linux",
        }
    }

    /// Every backend the repository has.
    pub const ALL: &'static [Platform] = &[Platform::Windows, Platform::Linux];

    /// The build host this platform's backend requires.
    pub const fn host(self) -> Host {
        match self {
            Self::Windows => Host::Windows,
            Self::Linux => Host::Linux,
        }
    }

    /// The backends a build host of `host` can run.
    ///
    /// `Host::Any` builds no native backend at all: a portable package is
    /// portable precisely because it needs none.
    pub const fn on(host: Host) -> &'static [Platform] {
        match host {
            Host::Any => &[],
            Host::Windows => &[Platform::Windows],
            Host::Linux => &[Platform::Linux],
        }
    }
}

/// What a package *is*, independent of where it builds.
///
/// [`Host`] answers "which machines verify this"; this answers "what may it
/// reach". The two are separate questions because composition tooling is built on
/// more than one native host at once, and a matrix that could only say "portable"
/// or "Windows" would force such a package to be called one or the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Genuinely portable: no native backend dependency, no platform branch.
    Portable,
    /// One native backend. Reaches the portable crates beneath it and no other
    /// backend.
    Backend(Platform),
    /// Composition or tooling that is intentionally built on more than one
    /// native host, and is therefore verified wherever the union of those hosts
    /// builds.
    Composition,
}

impl Kind {
    /// Whether this package may be verified on a host with no native backend.
    pub const fn is_portable(self) -> bool {
        matches!(self, Self::Portable)
    }

    /// Whether this package reaches a native backend by design.
    pub const fn is_composition(self) -> bool {
        matches!(self, Self::Composition)
    }

    /// The backend this package *is*, and `None` for anything else.
    pub const fn backend(self) -> Option<Platform> {
        match self {
            Self::Backend(platform) => Some(platform),
            _ => None,
        }
    }
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
    pub kind: Kind,
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
    // The published preset contract. Portable because a preset outside this
    // repository compiles it, and a contract only one platform can compile is not
    // one they can consume. It is deliberately free of GPUI, Windows APIs and
    // Tokio, so nothing about it is platform-shaped.
    "zup-preset-protocol",
    // The transport a preset and its host speak over. Portable for the same
    // reason the contract is: it is what makes a preset source portable, and a
    // transport only one platform can compile is not a portable one. It is
    // portable by using the operating system's own IPC rather than by
    // reimplementing any of it.
    "zup-preset-ipc",
    // The plugin contract: the WIT, its digest, and the limits a guest is held
    // to. Portable because it is pure data that both sides read, and a contract
    // only one platform can compile is not one a guest can be built against.
    "zup-plugin-abi",
    // The guest authoring SDK. Portable because a plugin is Wasm and every
    // plugin author builds one on the same platform; what it must never reach is
    // the engine that runs it, which the dependency graph gate enforces rather
    // than this matrix.
    "zup-plugin-sdk",
    // The attribute macro behind the preset SDK's settings. Portable because it
    // expands at compile time in the preset's own build and is platform-shaped
    // by whatever the preset targets.
    "zup-preset-sdk-macros",
    // The host side of the preset contract, with no engine behind it. Portable
    // because a preset author meets it in `zup preset dev` before they meet an
    // installer, and a state machine only one platform can run is not one they
    // can develop against.
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
    // The source half of `zup preset dev`: the Cargo project a preset author edits
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
/// What is still refused for these packages, unchanged: a native backend
/// dependency, a single-platform manifest target, `std::os::windows`, and a Win32
/// namespace. The branch may only choose between the two wrappers `process-wrap`
/// documents, and a package gains this by appearing here rather than by a line
/// added beside the code it silences.
pub const PORTABLE_PLATFORM_DELEGATING: &[&str] = &["zup-preset-host", "zup-preset-dev"];

/// Portable packages that must write a POSIX mode bit.
///
/// `std` has no portable spelling for "this file has to be runnable": the
/// executable bit is a permission, and permissions are per-platform. A preview
/// environment that stages a preset and then runs it needs that bit and has no
/// other way to have it.
///
/// This is narrower than [`PORTABLE_PLATFORM_DELEGATING`] in what it grants and
/// wider in none: it exempts one `cfg` spelling and one `std` import, and
/// nothing else. A package listed here may still not depend on a backend, may
/// not select a single-platform manifest table, may not name a Win32 namespace,
/// and is held to every concept table in [`crate::boundary`]. It does not become
/// a general escape hatch for platform code - `cfg(windows)` stays refused even
/// here, because a Windows permission model is a *backend* question rather than a
/// portable one, and that is the distinction this list exists to preserve.
pub const PORTABLE_POSIX_PERMISSIONS: &[&str] = &["zup-preview"];

/// The Windows backend.
///
/// Its own matrix rather than a member of a Windows-only list, because it is not
/// one product among several that happens to need a Windows host: it is the
/// mechanism layer every Windows-side decision below it is lowered onto, and the
/// rules that read this module have to be able to name it as *a* backend rather
/// than as "whatever the Windows one is".
pub const WINDOWS_BACKEND: &[&str] = &["zup-windows"];

/// The Linux backend.
pub const LINUX_BACKEND: &[&str] = &["zup-linux"];

/// The native backends, in [`Platform::ALL`] order.
pub const NATIVE_BACKENDS: &[&str] = &["zup-windows", "zup-linux"];

/// Crates that require a Windows build host and are not the Windows backend:
/// the composition CLI, the runtime an installer embeds, the native frontends,
/// and the small dispatcher a universal artifact starts through.
///
/// These are [`Kind::Composition`]: they reach a backend deliberately, because
/// reaching one is what they are for. What they may not do is pretend to be
/// portable, which is why the boundary holds them to the platform rules and
/// still refuses to let the list's membership decide what code they contain.
pub const WINDOWS_COMPOSITION: &[&str] = &[
    "zup-dispatch",
    "zup",
    "zup-installer",
    // The default installer interface, which is a GPUI application like any
    // other preset and is built by the toolchain rather than shipped inside an
    // installer as a link-time dependency.
    //
    // The preset SDK is here rather than in the portable core because its
    // platform is GPUI's, not Zup's: a preset author writes the same source on
    // every host and the GPUI stack decides which of them it builds for. The
    // transport underneath it is portable, and is `zup-preset-ipc`.
    "zup-preset-sdk",
    // The facade a preset author depends on. Windows-only for the same reason
    // the crate beneath it is: with no feature selected it is empty, and with
    // `preset` selected it carries GPUI. It is here rather than in the portable
    // core because a matrix is a statement about what builds on this machine,
    // and this is the crate whose `plugin` half does not build here at all.
    "zup-sdk",
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
];

/// Every matrix name, in declaration order.
pub fn names() -> Vec<&'static str> {
    MATRICES.iter().map(|matrix| matrix.name).collect()
}

/// Look up one matrix by name.
pub fn matrix(name: &str) -> Option<&'static Matrix> {
    MATRICES.iter().find(|matrix| matrix.name == name)
}

/// Whether `package` is genuinely portable.
///
/// True only for a [`Kind::Portable`] package, and false for composition
/// tooling: being *verifiable* on several hosts is not the same as being
/// platform-neutral, and the boundary's whole job depends on telling them
/// apart.
pub fn is_portable(package: &str) -> bool {
    kind_of(package).is_some_and(Kind::is_portable)
}

/// What `package` is, and `None` for a package no matrix claims.
pub fn kind_of(package: &str) -> Option<Kind> {
    MATRICES
        .iter()
        .find(|matrix| matrix.packages.contains(&package))
        .map(|matrix| matrix.kind)
}

/// Whether `package` reaches a native backend by design.
pub fn is_composition(package: &str) -> bool {
    kind_of(package).is_some_and(Kind::is_composition)
}

/// Whether `package` is one specific platform's mechanism layer.
pub fn is_backend(package: &str) -> bool {
    kind_of(package).is_some_and(|kind| kind.backend().is_some())
}

/// The packages a build host of `host` verifies.
///
/// What a CI job for that host builds and tests. Derived here rather than in a
/// workflow, so a job's coverage is a fact about the model: a package the model
/// does not classify for a host is a package that host does not claim to cover,
/// and a package it classifies is one the job cannot silently drop.
pub fn packages_for_host(host: Host) -> Vec<&'static str> {
    MATRICES
        .iter()
        .filter(|matrix| matrix.host == host)
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

/// The name a build host is called by, for a report that has to say which.
pub fn host_name(host: Host) -> &'static str {
    match host {
        Host::Any => "any",
        Host::Windows => "windows",
        Host::Linux => "linux",
    }
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
        .filter(|matrix| matrix.kind.is_portable())
        .flat_map(|matrix| matrix.packages.iter().copied())
        .collect()
}

/// Every native backend package, in [`Platform::ALL`] order.
///
/// The dependency-prefix table in [`crate::boundary`] and the isolation rules in
/// [`crate::graph`] both read this rather than naming a backend each, which is
/// what keeps a third platform from needing an edit in three places.
pub fn backends() -> Vec<&'static str> {
    Platform::ALL
        .iter()
        .map(|platform| platform.crate_name())
        .collect()
}

/// Every native backend package except `platform`'s own.
pub fn sibling_backends(platform: Platform) -> Vec<&'static str> {
    Platform::ALL
        .iter()
        .filter(|other| **other != platform)
        .map(|other| other.crate_name())
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

/// Whether a portable package may write a POSIX mode bit.
///
/// Separate from [`delegates_platform_lifecycle`] because the thing being
/// delegated is different: that one picks between a dependency's own two
/// wrappers, and this one spells a permission the standard library declines to
/// abstract. Merging them would make the second grant the first one's reasoning.
pub fn writes_posix_permissions(package: &str) -> bool {
    PORTABLE_POSIX_PERMISSIONS.contains(&package)
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
