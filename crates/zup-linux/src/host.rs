//! What this Linux host is.
//!
//! The portable selection model already knows how to ask three questions - is
//! this the native machine, can this host execute it another way, what version is
//! it - and already has closed types to answer them with
//! ([`HostArchitecture`], [`PlatformOs`], [`HostVersion`]). Nothing here
//! introduces a Linux-shaped identity for the portable model to parse; this
//! module reads the kernel's own answer and hands those existing types back.
//!
//! Two rules shape it, and they are the same two the Windows backend obeys:
//!
//! - **Ask the kernel, never infer.** An architecture read out of an environment
//!   variable or a directory name is a guess that happens to be right on the
//!   machine somebody tested on.
//! - **Do not confuse the process with the machine.** A 32-bit process runs on
//!   every supported host precisely because it is not the host's architecture, so
//!   selection reads the machine rather than the running image.
//!
//! Linux's answer to the second rule is simpler than Windows': there is no Wow64
//! layer to unwind, so `uname`'s machine *is* the machine. That is a property of
//! the kernel interface, not an assumption - which is why this module reads it
//! rather than deriving an answer from the compiled-in target.
//!
//! `rustix` supplies `uname` as a safe call, so nothing here needs `unsafe` to
//! reach the kernel.
//!
//! [`HostArchitecture`]: zup_artifact::HostArchitecture
//! [`PlatformOs`]: zup_artifact::PlatformOs
//! [`HostVersion`]: zup_artifact::HostVersion

use zup_artifact::{HostArchitecture, HostExecution, HostVersion, PlatformOs};

/// Failures produced while reading this host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("the kernel reported the machine architecture `{0}`, which zup does not model")]
    UnknownArchitecture(String),
}

/// The machine architecture of the machine this process is running on.
///
/// Read from `uname`, which describes the kernel and therefore the machine, not
/// the image this program happens to have been compiled for.
pub fn native_architecture() -> Result<HostArchitecture, HostError> {
    let reported = uname().machine().to_string_lossy().into_owned();
    // `uname` spells the machine with the architecture's own short name, which is
    // not always the triple's spelling: a triple writes `x86_64` and an i686
    // kernel says `i686`. Mapping through the model's own `from_name` keeps every
    // alias in one place rather than guessing at it here.
    HostArchitecture::from_name(&reported).ok_or(HostError::UnknownArchitecture(reported))
}

/// This host's kernel version.
///
/// The Linux analogue of a Windows build number, and the value a variant's
/// minimum host requirement is compared against. A release with no leading
/// number at all is reported as unknown, which fails such a requirement closed
/// rather than waving it through.
pub fn host_version() -> Option<HostVersion> {
    leading_version(&uname().release().to_string_lossy())
}

/// The `major.minor` a kernel release names.
///
/// A release is `major.minor` with an arbitrarily long suffix, and the suffix is
/// where every distribution puts its own identity: `6.8.0-1028-azure` and
/// `6.8.0-45-generic` are the same kernel with different builds behind it. Only
/// the two leading numbers are comparable, and only they are read.
fn leading_version(release: &str) -> Option<HostVersion> {
    let mut numbers = release
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(2)
        .filter_map(|part| part.parse::<u32>().ok());
    Some(HostVersion::new(
        numbers.next()?,
        numbers.next().unwrap_or(0),
        0,
    ))
}

/// The architectures this host can execute besides its own, most preferred first.
///
/// There is no interface to ask, so the answer is the kernel's own long-standing
/// compatibility support: a 64-bit kernel runs every 32-bit program of its own
/// architecture family. Nothing here is an emulation layer in the sense
/// `HostExecution` means, so these are reported as natively executable - which
/// is what they are.
///
/// A host whose own architecture is not modelled reports nothing rather than
/// guessing, which fails a variant's selection closed.
pub fn additional_architectures(native: HostArchitecture) -> Vec<HostArchitecture> {
    match native {
        HostArchitecture::X86_64 => vec![HostArchitecture::X86],
        HostArchitecture::Arm64 => vec![HostArchitecture::Arm],
        HostArchitecture::X86 | HostArchitecture::Arm => Vec::new(),
    }
}

/// What this host can execute, in the portable model's vocabulary.
pub fn host_execution() -> HostExecution {
    let native = native_architecture().unwrap_or(HostArchitecture::X86_64);
    HostExecution {
        os: PlatformOs::Linux,
        native,
        emulated: additional_architectures(native),
        version: host_version(),
    }
}

/// The kernel's identification of the running system.
///
/// `uname` takes no arguments and has no failure mode: there is no buffer to get
/// wrong and no permission to be denied, so it always answers. `rustix` returns
/// accessors over a fixed-size kernel structure rather than a copy, which is why
/// this returns the value and the callers read fields off it.
fn uname() -> rustix::system::Uname {
    rustix::system::uname()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that cannot describe itself is a selection failure, not a reason to
    /// assume the most common machine. The assertion is about the machine this
    /// test runs on, which is the only claim it can honestly make.
    #[test]
    fn this_host_reports_an_architecture_zup_models() {
        let native = native_architecture().expect("the kernel names this machine");
        assert!(matches!(
            native,
            HostArchitecture::X86
                | HostArchitecture::X86_64
                | HostArchitecture::Arm
                | HostArchitecture::Arm64
        ));
        assert!(
            !additional_architectures(native).contains(&native),
            "{native} cannot execute itself *additional* to itself"
        );
        assert_eq!(host_execution().os, PlatformOs::Linux);
    }

    /// The kernel release is `major.minor` plus a suffix, and the suffix is where
    /// every distribution puts its own identity. Reading it as a number would make
    /// `6.8.0-1028-azure` a different kernel version from `6.8.0-45-generic`, so
    /// two identical kernels would fail each other's minimum-version requirement.
    #[rstest::rstest]
    #[case::vendor_suffix("6.8.0-45-generic", Some(HostVersion::new(6, 8, 0)))]
    #[case::azure_build("6.8.0-1028-azure", Some(HostVersion::new(6, 8, 0)))]
    #[case::two_components("5.15", Some(HostVersion::new(5, 15, 0)))]
    #[case::no_patch("6.1", Some(HostVersion::new(6, 1, 0)))]
    #[case::nothing_parsable("unknown", None)]
    fn a_version_reads_two_leading_numbers_and_ignores_the_rest(
        #[case] release: &str,
        #[case] expected: Option<HostVersion>,
    ) {
        assert_eq!(leading_version(release), expected, "{release}");
    }

    /// An architecture zup does not model must be refused rather than folded into
    /// a neighbouring one: selecting a variant for the wrong machine is a
    /// compromise, and refusing is a supported answer.
    #[test]
    fn an_unmodelled_machine_is_refused_rather_than_guessed() {
        assert_eq!(
            HostArchitecture::from_name("x86_64"),
            Some(HostArchitecture::X86_64)
        );
        assert_eq!(
            HostArchitecture::from_name("aarch64"),
            Some(HostArchitecture::Arm64)
        );
        assert_eq!(HostArchitecture::from_name("riscv64"), None);
        assert_eq!(
            native_architecture().is_ok(),
            HostArchitecture::from_name(&native_architecture_name()).is_some(),
            "the two answers cannot disagree about the same kernel"
        );
    }

    fn native_architecture_name() -> String {
        uname().machine().to_string_lossy().into_owned()
    }
}
