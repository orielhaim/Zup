//! Whether a binary can serve a claim made about it.
//!
//! A `TargetTriple` in a manifest, a toolchain descriptor, or a release manifest
//! is a claim; the file it is about makes its own, and where the two disagree the
//! file is right. The comparison is partial by design: architecture always,
//! operating system only when the file states one, and never a vendor or an ABI,
//! because no format zup reads records either.

use thiserror::Error;
use zup_core::{Frontend, TargetArchitecture, TargetOperatingSystem, TargetTriple};

use crate::{Architectures, BinaryArchitecture, Executable, ProgramKind};

/// Every variant names a field the file stated and the claim contradicted, which
/// is why there is none for a field neither of them had an opinion about.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetRefusal {
    #[error(
        "declared target does not match binary: the binary is {found}, and the target is {declared}"
    )]
    Architecture {
        declared: TargetArchitecture,
        found: Architectures,
    },
    #[error(
        "declared target does not match binary: the binary is for {found}, and the target is for {declared}"
    )]
    OperatingSystem {
        declared: TargetOperatingSystem,
        found: TargetOperatingSystem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FrontendRefusal {
    #[error(
        "declared frontend does not match binary: the binary is {found}, and the frontend is {declared}"
    )]
    ProgramKind {
        declared: Frontend,
        found: ProgramKind,
    },
}

impl Executable {
    pub fn matches_target(&self, target: &TargetTriple) -> bool {
        self.refuse_target(target).is_ok()
    }

    /// Architecture first: it is the one field every executable states, and there
    /// is no such thing as the right operating system on the wrong machine.
    pub fn refuse_target(&self, target: &TargetTriple) -> Result<(), TargetRefusal> {
        if !BinaryArchitecture::of_target(target).is_some_and(|machine| self.carries(machine)) {
            return Err(TargetRefusal::Architecture {
                declared: target.architecture(),
                found: self.architectures().clone(),
            });
        }
        match self.operating_system() {
            // A file that states nothing leaves the claim unconstrained.
            None => Ok(()),
            Some(found) if same_system(found, target.operating_system()) => Ok(()),
            Some(found) => Err(TargetRefusal::OperatingSystem {
                declared: target.operating_system(),
                found,
            }),
        }
    }

    pub fn matches_frontend(&self, declared: Frontend) -> bool {
        self.refuse_frontend(declared).is_ok()
    }

    /// A file whose format records no subsystem contradicts nothing: there is no
    /// signal to contradict with, and refusing a Mach-O or an ELF for saying
    /// nothing about windows would be refusing the format rather than the file.
    pub fn refuse_frontend(&self, declared: Frontend) -> Result<(), FrontendRefusal> {
        match self.program() {
            None => Ok(()),
            Some(found) if serves(declared, found) => Ok(()),
            Some(found) => Err(FrontendRefusal::ProgramKind { declared, found }),
        }
    }
}

/// A console program serves a headless frontend because a headless frontend is a
/// console program that also promises to speak a protocol rather than draw
/// anything, and the promise is the caller's to keep.
fn serves(declared: Frontend, found: ProgramKind) -> bool {
    matches!(
        (declared, found),
        (Frontend::Gui, ProgramKind::Windowed)
            | (Frontend::Console | Frontend::Headless, ProgramKind::Console)
    )
}

/// A deployment target is dropped rather than compared: a triple's is the minimum
/// version a build supports and an executable's is nothing at all.
///
/// `Darwin` is macOS's other spelling. `target-lexicon` parses
/// `x86_64-apple-darwin` - what a Rust target triple for macOS says - as `Darwin`,
/// and an `LC_BUILD_VERSION` of `1` means `MacOSX`.
fn same_system(found: TargetOperatingSystem, declared: TargetOperatingSystem) -> bool {
    let (found, declared) = (plain_system(found), plain_system(declared));
    match (found, declared) {
        (TargetOperatingSystem::MacOSX(_), TargetOperatingSystem::Darwin(_))
        | (TargetOperatingSystem::Darwin(_), TargetOperatingSystem::MacOSX(_)) => true,
        _ => found == declared,
    }
}

fn plain_system(system: TargetOperatingSystem) -> TargetOperatingSystem {
    use TargetOperatingSystem::{Darwin, IOS, MacOSX, TvOS, VisionOS, WatchOS, XROS};
    match system {
        Darwin(_) => Darwin(None),
        IOS(_) => IOS(None),
        MacOSX(_) => MacOSX(None),
        TvOS(_) => TvOS(None),
        VisionOS(_) => VisionOS(None),
        WatchOS(_) => WatchOS(None),
        XROS(_) => XROS(None),
        other => other,
    }
}
