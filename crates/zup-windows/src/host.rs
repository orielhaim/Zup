//! What this host can execute.
//!
//! The portable selection model knows three answers, native, supported
//! compatibility or emulation, and unsupported. This module supplies the
//! knowledge behind them, and it is the only place that knows how Windows
//! reports a machine's own architecture or what it can additionally execute.
//!
//! Two rules shape the implementation:
//!
//! - **Never guess from the environment.** A machine's architecture is a
//!   property the host reports about the running process and the machine, not
//!   something inferred from a variable, a path, or a filename.
//! - **Do not trust the dispatcher's own architecture.** A 32-bit launcher runs
//!   on every supported host precisely because it is not the host's
//!   architecture, so selection reads the native machine, not the process.

use windows_link::link;
use zup_artifact::{HostArchitecture, HostExecution, HostVersion, PlatformOs};

type Bool = i32;
type Word = u16;
type Dword = u32;
type Status = i32;
type Handle = *mut core::ffi::c_void;

link!("kernel32.dll" "system" fn GetCurrentProcess() -> Handle);
link!("kernel32.dll" "system" fn IsWow64Process2(process: Handle, process_machine: *mut Word, native_machine: *mut Word) -> Bool);
link!("kernel32.dll" "system" fn LoadLibraryExW(name: *const u16, file: Handle, flags: Dword) -> Handle);
link!("kernel32.dll" "system" fn GetProcAddress(module: Handle, name: *const u8) -> *const core::ffi::c_void);
link!("ntdll.dll" "system" fn RtlGetVersion(version: *mut RtlOsVersionInfo) -> Status);

const IMAGE_FILE_MACHINE_UNKNOWN: Word = 0;
const IMAGE_FILE_MACHINE_I386: Word = 0x014c;
const IMAGE_FILE_MACHINE_AMD64: Word = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: Word = 0xaa64;

/// The first Windows version that executes x64 code on an ARM64 machine.
const WINDOWS_11: HostVersion = HostVersion {
    major: 10,
    minor: 0,
    patch: 22000,
};

/// The machine types a process image can be built for, which is not the same
/// question as what the host can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMachines {
    /// The machine type the running process image is built for.
    pub process_machine: HostArchitecture,
    /// The machine type of the machine the process is running on.
    pub native_machine: HostArchitecture,
}

/// The machine type of the machine this process is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeMachine {
    pub architecture: HostArchitecture,
}

/// Failures produced while reading the host.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("the host did not report a machine architecture this build understands")]
    UnknownMachine,
    #[error("the host did not report a process architecture this build understands")]
    UnknownProcessMachine,
}

/// Read the running process's own machine type and the machine's native one.
///
/// This is the only correct source for both. A 32-bit process on a 64-bit
/// machine reports an *unknown* process machine and a 64-bit native machine,
/// which is the case a universal dispatcher runs in and the case a naive reading
/// of the process machine would get wrong.
pub fn process_machines() -> Result<ProcessMachines, HostError> {
    let mut process_machine: Word = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine: Word = IMAGE_FILE_MACHINE_UNKNOWN;
    let ok = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
    };
    if ok == 0 {
        return Err(HostError::UnknownMachine);
    }
    let native = machine(native_machine).ok_or(HostError::UnknownMachine)?;
    let process = match process_machine {
        // An unknown process machine means "the same as the machine's", which is
        // what a natively-built process reports.
        IMAGE_FILE_MACHINE_UNKNOWN => native,
        other => machine(other).ok_or(HostError::UnknownProcessMachine)?,
    };
    Ok(ProcessMachines {
        process_machine: process,
        native_machine: native,
    })
}

/// The machine type of the machine this process is running on, which is what a
/// variant is selected against.
pub fn native_machine() -> Result<NativeMachine, HostError> {
    Ok(NativeMachine {
        architecture: process_machines()?.native_machine,
    })
}

const fn machine(value: Word) -> Option<HostArchitecture> {
    match value {
        IMAGE_FILE_MACHINE_I386 => Some(HostArchitecture::X86),
        IMAGE_FILE_MACHINE_AMD64 => Some(HostArchitecture::X86_64),
        IMAGE_FILE_MACHINE_ARM64 => Some(HostArchitecture::Arm64),
        _ => None,
    }
}

#[repr(C)]
struct RtlOsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform_id: u32,
    service_pack: [u16; 128],
}

/// The host's own version.
///
/// The compatibility interface the process is manifested for lies about this on
/// purpose, so the version is read from the interface that reports the real one.
/// A version that cannot be read is reported as unknown, which fails a variant's
/// minimum host requirement closed rather than waving it through.
pub fn host_version() -> Option<HostVersion> {
    let mut info = RtlOsVersionInfo {
        size: std::mem::size_of::<RtlOsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform_id: 0,
        service_pack: [0; 128],
    };
    if unsafe { RtlGetVersion(&mut info) } != 0 {
        return None;
    }
    Some(HostVersion::new(
        info.major as u32,
        info.minor as u32,
        info.build as u32,
    ))
}

/// What the host reports about a machine type it did not build this process for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineSupport {
    /// The host reports native support.
    Supported,
    /// The host reports it does not support the machine type.
    Unsupported,
    /// The host does not report anything about it.
    Unknown,
}

impl MachineSupport {
    /// Whether a variant for this machine type may be selected on this host.
    ///
    /// An unreported answer is resolved against the documented capability
    /// boundary rather than assumed either way, so the decision is stated rather
    /// than accidental.
    pub fn permits_selection(self, version: Option<&HostVersion>) -> bool {
        match self {
            Self::Supported => true,
            Self::Unsupported => false,
            Self::Unknown => version.is_some_and(|version| version.at_least(&WINDOWS_11)),
        }
    }
}

type IsMachineTypeSupported = unsafe extern "system" fn(Word, Dword) -> Bool;

const IMAGE_FILE_MACHINE_AMD64_NATIVE: Dword = 0x0000_0004;

/// Ask the host what it can execute natively for `machine_type`.
///
/// The interface is resolved by name from the modules that may export it,
/// because a host without it answers nothing rather than answering "no". The
/// difference matters: an interface that reports "no" and an interface that is
/// absent lead to different decisions.
pub fn machine_support(machine_type: Word) -> MachineSupport {
    let Some(probe) = is_machine_type_supported() else {
        return MachineSupport::Unknown;
    };
    if unsafe { probe(machine_type, IMAGE_FILE_MACHINE_AMD64_NATIVE) } != 0 {
        MachineSupport::Supported
    } else {
        MachineSupport::Unsupported
    }
}

fn is_machine_type_supported() -> Option<IsMachineTypeSupported> {
    // The interface is reached through an API set, so a module that is not
    // already loaded has to be opened by name. An export found in any of these is
    // the same function; which one answers is the host's business.
    const CANDIDATES: &[&str] = &[
        "api-ms-win-core-sys-l2-2-0.dll",
        "api-ms-win-core-sys-l2-1-0.dll",
        "kernelbase.dll",
    ];
    for candidate in CANDIDATES {
        let wide: Vec<u16> = candidate.encode_utf16().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0x0000_0800) };
        if module.is_null() {
            continue;
        }
        let address = unsafe { GetProcAddress(module, c"IsMachineTypeSupported".as_ptr().cast()) };
        if !address.is_null() {
            // The resolved address is a `BOOL (WORD, DWORD)` under the system
            // calling convention, which the function-pointer type already names.
            return Some(unsafe {
                std::mem::transmute::<*const core::ffi::c_void, IsMachineTypeSupported>(address)
            });
        }
    }
    None
}

/// The architectures a host of `native` can additionally execute, most preferred
/// first.
///
/// x86 code runs on every 64-bit host. x64 code on an ARM64 host is a later
/// capability, asked of the host and resolved against the documented boundary
/// when the host says nothing, which is what makes the answer on an older host
/// deliberate.
pub fn emulated_architectures(
    native: HostArchitecture,
    version: Option<&HostVersion>,
) -> Vec<HostArchitecture> {
    match native {
        HostArchitecture::X86_64 => vec![HostArchitecture::X86],
        HostArchitecture::Arm64 => {
            let mut order = vec![HostArchitecture::X86];
            if machine_support(IMAGE_FILE_MACHINE_AMD64).permits_selection(version) {
                // Prefer the wider machine when both are available, which is the
                // same tie-break the portable selector applies.
                order.insert(0, HostArchitecture::X86_64);
            }
            order
        }
        HostArchitecture::X86 | HostArchitecture::Arm => Vec::new(),
    }
}

/// What this host can execute, in the portable model's vocabulary.
pub fn host_execution() -> HostExecution {
    let native = native_machine()
        .map(|machine| machine.architecture)
        .unwrap_or(HostArchitecture::X86_64);
    let version = host_version();
    HostExecution {
        os: PlatformOs::Windows,
        native,
        emulated: emulated_architectures(native, version.as_ref()),
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_host_reports_itself_and_a_machine_type_zup_models() {
        let machines = process_machines().expect("the host reports its machine types");
        assert!(matches!(
            machines.native_machine,
            HostArchitecture::X86_64 | HostArchitecture::Arm64
        ));
        assert!(matches!(
            machines.process_machine,
            HostArchitecture::X86 | HostArchitecture::X86_64 | HostArchitecture::Arm64
        ));
    }

    #[test]
    fn an_emulated_list_never_contains_the_native_architecture() {
        for native in [
            HostArchitecture::X86,
            HostArchitecture::X86_64,
            HostArchitecture::Arm,
            HostArchitecture::Arm64,
        ] {
            let emulated = emulated_architectures(native, Some(&HostVersion::new(11, 0, 0)));
            assert!(
                !emulated.contains(&native),
                "{native} cannot emulate itself"
            );
        }
        // 32-bit code runs on every 64-bit host, whatever that host is.
        for native in [HostArchitecture::X86_64, HostArchitecture::Arm64] {
            assert!(
                emulated_architectures(native, Some(&HostVersion::new(11, 0, 0)))
                    .contains(&HostArchitecture::X86),
                "{native} executes 32-bit code"
            );
        }
        // A 32-bit host executes nothing but its own architecture.
        for native in [HostArchitecture::X86, HostArchitecture::Arm] {
            assert!(
                emulated_architectures(native, Some(&HostVersion::new(11, 0, 0))).is_empty(),
                "{native} executes only its own architecture"
            );
        }
    }
}
