//! Which GitHub-hosted runner builds a target natively.
//!
//! # Why this is a function and not a lookup table
//!
//! Because "Windows" stopped meaning x64 in 2025 and will stop meaning much else
//! later. A table keyed on the operating system would have kept working silently while
//! being wrong, which is the worst failure mode for a table whose entire job is to be
//! right about a machine's architecture. So the mapping is by triple, and it says what
//! it knows:
//!
//! ```text
//! x86_64 windows    → windows-latest
//! aarch64 windows   → windows-11-arm
//! i686 windows      → no native runner; cross-compiled on windows-latest
//! x86_64 linux      → ubuntu-latest
//! aarch64 linux     → ubuntu-24.04-arm
//! x86_64 macos      → macos-15-intel
//! aarch64 macos     → macos-latest
//! ```
//!
//! # The rule that matters most
//!
//! **The runner's architecture is never the artifact's identity.** A job that runs on
//! `windows-latest` and produces a `aarch64-pc-windows-msvc` artifact is a
//! cross-compile, and the release manifest says `aarch64-pc-windows-msvc` because
//! that is what the file is. Conflating the two is how a project ends up with an
//! `arm64` release that is really an x64 build with a misleading name.

/// A runner label and whether it is the architecture the target wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runner {
    /// The `runs-on` label.
    pub label: &'static str,
    /// Whether this runner's own architecture matches the target's.
    pub native: bool,
}

impl Runner {
    /// The label, for a report.
    pub const fn label(&self) -> &'static str {
        self.label
    }
}

/// The Windows ARM64 label, with a comment for the generated file.
pub const WINDOWS_ARM: &str = "windows-11-arm";

/// The Windows ARM64 label with the Visual Studio 2026 toolchain.
pub const WINDOWS_ARM_VS2026: &str = "windows-11-vs2026-arm";

/// The native runner for `triple`, if one exists.
///
/// `None` means GitHub has no hosted machine of that shape, which is not a
/// refusal: the workflow then cross-compiles on the nearest runner and says so.
pub fn native_runner(triple: &str) -> Option<Runner> {
    let (arch, os) = split(triple)?;
    let runner = match (os, arch) {
        ("windows", "x86_64") => Runner {
            label: "windows-latest",
            native: true,
        },
        ("windows", "aarch64") => Runner {
            label: WINDOWS_ARM,
            native: true,
        },
        ("linux", "x86_64") => Runner {
            label: "ubuntu-latest",
            native: true,
        },
        ("linux", "aarch64") => Runner {
            label: "ubuntu-24.04-arm",
            native: true,
        },
        // `macos-latest` is the ARM64 image now that Apple silicon is the default
        // Mac, so an Intel target has to name the Intel runner explicitly.
        ("macos", "aarch64") => Runner {
            label: "macos-latest",
            native: true,
        },
        ("macos", "x86_64") => Runner {
            label: "macos-15-intel",
            native: true,
        },
        _ => return None,
    };
    Some(runner)
}

/// The runner a cross-compile for `triple` happens on, and where it happens from.
pub fn cross_runner(triple: &str) -> &'static str {
    match split(triple).map(|(_, os)| os) {
        Some("windows") => "windows-latest",
        Some("macos") => "macos-latest",
        _ => "ubuntu-latest",
    }
}

/// Whether a triple is a Windows target.
///
/// The generated workflow needs this to choose a compose runner, because
/// composition writes a PE and only a Windows machine can do that.
pub fn is_windows(triple: &str) -> bool {
    split(triple).is_some_and(|(_, os)| os == "windows")
}

/// Split a target triple into its architecture and operating system.
///
/// The operating system is normalized: a target triple spells Apple's as
/// `darwin`, and every report, matrix key, and diagnostic in zup spells it
/// `macos`. Normalizing once here is what keeps `is_windows` and the runner
/// table from disagreeing with each other about what a triple is.
fn split(triple: &str) -> Option<(&str, &str)> {
    let mut parts = triple.split('-');
    let arch = parts.next()?;
    let _vendor = parts.next()?;
    let os = parts.next()?;
    Some((arch, normalize_os(os)))
}

fn normalize_os(os: &str) -> &str {
    match os {
        "darwin" => "macos",
        "win32" => "windows",
        other => other,
    }
}
