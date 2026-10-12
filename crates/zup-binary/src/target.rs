//! Whether a binary can serve a claim made about it.
//!
//! A `Target` in a manifest, a toolchain descriptor, or a release manifest
//! is a claim; the file it is about makes its own, and where the two disagree the
//! file is right. The comparison is partial by design: architecture always,
//! operating system only when the file states one, because no format zup reads
//! records a vendor or an ABI.

use thiserror::Error;
use zup_core::{Frontend, Target, TargetArchitecture, TargetOperatingSystem};

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
    pub fn matches_target(&self, target: &Target) -> bool {
        self.refuse_target(target).is_ok()
    }

    /// Architecture first: it is the one field every executable states, and there
    /// is no such thing as the right operating system on the wrong machine.
    pub fn refuse_target(&self, target: &Target) -> Result<(), TargetRefusal> {
        if !self.carries(BinaryArchitecture::of_target(target)) {
            return Err(TargetRefusal::Architecture {
                declared: target.architecture(),
                found: self.architectures().clone(),
            });
        }
        match self.operating_system() {
            // A file that states nothing leaves the claim unconstrained.
            None => Ok(()),
            Some(found) if found == target.operating_system() => Ok(()),
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
