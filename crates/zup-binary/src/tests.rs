//! Small hand-built fixtures, one per format zup reads.
//!
//! These test the claims zup makes about a file it has read and the ways a read
//! can fail, not `object`'s parser. Building the bytes here means every field a
//! test depends on is a named one in this file.

use zup_core::TargetTriple;

use super::*;

const MACHINE_I386: u16 = 0x014c;
const MACHINE_AMD64: u16 = 0x8664;
const MACHINE_ARM64: u16 = 0xaa64;
/// `IMAGE_FILE_MACHINE_R4000`: a real machine type zup cannot name.
const MACHINE_SPARC: u16 = 0x0266;

const SUBSYSTEM_GUI: u16 = 2;
const SUBSYSTEM_CUI: u16 = 3;
/// `IMAGE_SUBSYSTEM_EFI_APPLICATION`: a real subsystem that is neither.
const SUBSYSTEM_EFI: u16 = 10;

const EM_386: u16 = 3;
const EM_ARM: u16 = 40;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
/// `EM_SPARCV9`: real, and one zup cannot name.
const EM_SPARCV9: u16 = 43;

const CPU_X86_64: u32 = 0x0100_0007;
const CPU_ARM64: u32 = 0x0100_000c;

/// A PE image: DOS stub, NT headers, one empty section.
///
/// Not loadable, and not meant to be - a fixture that had to be a real build
/// would be testing the linker.
fn pe(machine: u16, subsystem: u16) -> Vec<u8> {
    const OPTIONAL_HEADER: usize = 240;
    const SECTION: usize = 40;
    let mut bytes = vec![0u8; 0x40 + 4 + 20 + OPTIONAL_HEADER + SECTION];

    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
    let coff = 0x44;
    put(&mut bytes, coff, &machine.to_le_bytes());
    put(&mut bytes, coff + 2, &1u16.to_le_bytes());
    put(
        &mut bytes,
        coff + 16,
        &(OPTIONAL_HEADER as u16).to_le_bytes(),
    );
    let optional = coff + 20;
    put(&mut bytes, optional, &0x20bu16.to_le_bytes());
    // `Subsystem` is at the same offset in PE32 and PE32+; the layouts differ only
    // below it.
    put(&mut bytes, optional + 68, &subsystem.to_le_bytes());
    bytes
}

/// A PE image with `IMAGE_FILE_DLL` set.
fn pe_library(machine: u16) -> Vec<u8> {
    let mut bytes = pe(machine, SUBSYSTEM_CUI);
    put(&mut bytes, 0x44 + 18, &0x2000u16.to_le_bytes());
    bytes
}

/// What an ELF image is, which its file type does not say.
#[derive(Clone, Copy)]
enum ElfKind {
    /// `ET_EXEC`: a program whose load address is in the header.
    Fixed,
    /// `ET_DYN` with a `PT_INTERP`, which is what every distribution builds.
    PositionIndependent,
    /// `ET_DYN` with no interpreter.
    Shared,
}

/// A 64-bit little-endian ELF image.
fn elf64(machine: u16, os_abi: u8, kind: ElfKind) -> Vec<u8> {
    const HEADER: usize = 64;
    const PROGRAM_HEADER: usize = 56;
    let interpreter = matches!(kind, ElfKind::PositionIndependent);
    let mut bytes = vec![0u8; HEADER + usize::from(interpreter) * PROGRAM_HEADER];

    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2; // ELFCLASS64
    bytes[5] = 1; // ELFDATA2LSB
    bytes[6] = 1; // EV_CURRENT
    bytes[7] = os_abi;
    put(
        &mut bytes,
        16,
        &(if matches!(kind, ElfKind::Fixed) {
            2u16
        } else {
            3u16
        })
        .to_le_bytes(),
    );
    put(&mut bytes, 18, &machine.to_le_bytes());
    put(&mut bytes, 20, &1u32.to_le_bytes());
    put(&mut bytes, 32, &(HEADER as u64).to_le_bytes());
    put(&mut bytes, 52, &(HEADER as u16).to_le_bytes());
    if interpreter {
        put(&mut bytes, 54, &(PROGRAM_HEADER as u16).to_le_bytes());
        put(&mut bytes, 56, &1u16.to_le_bytes());
        put(&mut bytes, 64, &object::elf::PT_INTERP.0.to_le_bytes());
    }
    bytes
}

/// A 64-bit Mach-O image. `platform` is `LC_BUILD_VERSION`'s platform value, or
/// `None` for an image with no such load command.
fn macho64(cpu: u32, platform: Option<u32>) -> Vec<u8> {
    const HEADER: usize = 32;
    const BUILD_VERSION: usize = 24;
    let commands = usize::from(platform.is_some()) * BUILD_VERSION;
    let mut bytes = vec![0u8; HEADER + commands];

    put(&mut bytes, 0, &0xfeed_facfu32.to_le_bytes());
    put(&mut bytes, 4, &cpu.to_le_bytes());
    put(&mut bytes, 12, &2u32.to_le_bytes()); // MH_EXECUTE
    if let Some(platform) = platform {
        put(&mut bytes, 16, &1u32.to_le_bytes());
        put(&mut bytes, 20, &(BUILD_VERSION as u32).to_le_bytes());
        put(&mut bytes, HEADER, &0x32u32.to_le_bytes());
        put(
            &mut bytes,
            HEADER + 4,
            &(BUILD_VERSION as u32).to_le_bytes(),
        );
        put(&mut bytes, HEADER + 8, &platform.to_le_bytes());
    }
    bytes
}

/// A Mach-O universal binary over `members`, in the order given.
fn fat_macho64(members: &[Vec<u8>]) -> Vec<u8> {
    const HEADER: usize = 8;
    const FAT_ARCH: usize = 32;
    /// Members are page-aligned in every file a tool produces.
    const STRIDE: u64 = 0x1000;

    let start = (HEADER as u64 + FAT_ARCH as u64 * members.len() as u64).div_ceil(STRIDE) * STRIDE;
    let offsets: Vec<u64> = (0..members.len() as u64)
        .map(|index| start + index * STRIDE)
        .collect();
    let size = offsets.last().map_or(start, |last| {
        last + members.last().map_or(0, |member| member.len() as u64)
    });

    let mut bytes = vec![0u8; size as usize];
    // FAT_MAGIC_64 is big-endian whatever the members themselves are.
    bytes[..4].copy_from_slice(&0xcafe_babfu32.to_be_bytes());
    bytes[4..8].copy_from_slice(&(members.len() as u32).to_be_bytes());
    for (index, member) in members.iter().enumerate() {
        let entry = HEADER + index * FAT_ARCH;
        let cpu = u32::from_le_bytes(member[4..8].try_into().expect("four bytes"));
        bytes[entry..entry + 4].copy_from_slice(&cpu.to_be_bytes());
        bytes[entry + 8..entry + 16].copy_from_slice(&offsets[index].to_be_bytes());
        bytes[entry + 16..entry + 24].copy_from_slice(&(member.len() as u64).to_be_bytes());
        bytes[entry + 24..entry + 28].copy_from_slice(&12u32.to_be_bytes());
        let at = offsets[index] as usize;
        bytes[at..at + member.len()].copy_from_slice(member);
    }
    bytes
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

fn target(triple: &str) -> TargetTriple {
    TargetTriple::parse(triple).expect("a well-known triple")
}

fn inspect(bytes: &[u8]) -> Result<Executable, InspectError> {
    Executable::from_bytes(bytes)
}

#[test]
fn a_pe_names_its_format_its_machine_and_windows() {
    let executable = inspect(&pe(MACHINE_AMD64, SUBSYSTEM_CUI)).expect("a PE");
    assert_eq!(executable.format(), BinaryFormat::Pe);
    assert_eq!(executable.architecture(), Some(BinaryArchitecture::X86_64));
    assert_eq!(executable.program(), Some(ProgramKind::Console));
    assert_eq!(
        executable.operating_system(),
        Some(TargetOperatingSystem::Windows)
    );
}

#[test]
fn a_pe_subsystem_decides_the_program_kind_and_nothing_else_does() {
    for (subsystem, kind) in [
        (SUBSYSTEM_GUI, Some(ProgramKind::Windowed)),
        (SUBSYSTEM_CUI, Some(ProgramKind::Console)),
        (SUBSYSTEM_EFI, None),
    ] {
        let executable = inspect(&pe(MACHINE_AMD64, subsystem)).expect("a PE");
        assert_eq!(executable.program(), kind, "subsystem {subsystem}");
    }
}

#[test]
fn every_pe_machine_zup_targets_is_read_as_itself() {
    for (machine, architecture) in [
        (MACHINE_I386, BinaryArchitecture::X86_32),
        (MACHINE_AMD64, BinaryArchitecture::X86_64),
        (MACHINE_ARM64, BinaryArchitecture::Arm64),
    ] {
        let executable = inspect(&pe(machine, SUBSYSTEM_CUI)).expect("a PE");
        assert_eq!(executable.architecture(), Some(architecture));
        assert!(executable.carries(architecture));
    }
}

#[test]
fn an_elf_names_its_format_and_records_no_subsystem() {
    let executable = inspect(&elf64(EM_X86_64, 0, ElfKind::Fixed)).expect("an ELF");
    assert_eq!(executable.format(), BinaryFormat::Elf);
    assert_eq!(executable.architecture(), Some(BinaryArchitecture::X86_64));
    assert_eq!(
        executable.program(),
        None,
        "ELF has no window/terminal field, and inventing one would be a guess"
    );
}

#[test]
fn every_elf_machine_zup_targets_is_read_as_itself() {
    for (machine, architecture) in [
        (EM_386, BinaryArchitecture::X86_32),
        (EM_ARM, BinaryArchitecture::Arm32),
        (EM_X86_64, BinaryArchitecture::X86_64),
        (EM_AARCH64, BinaryArchitecture::Arm64),
    ] {
        let executable = inspect(&elf64(machine, 0, ElfKind::Fixed)).expect("an ELF");
        assert_eq!(executable.architecture(), Some(architecture));
    }
}

#[test]
fn an_elf_names_an_operating_system_only_when_its_osabi_does() {
    // The default OSABI is System V's "unspecified", so it says nothing.
    assert_eq!(
        inspect(&elf64(EM_X86_64, 0, ElfKind::Fixed))
            .expect("an ELF")
            .operating_system(),
        None
    );
    assert_eq!(
        inspect(&elf64(EM_X86_64, 3, ElfKind::Fixed))
            .expect("an ELF")
            .operating_system(),
        Some(TargetOperatingSystem::Linux)
    );
    assert_eq!(
        inspect(&elf64(EM_AARCH64, 9, ElfKind::Fixed))
            .expect("an ELF")
            .operating_system(),
        Some(TargetOperatingSystem::Freebsd)
    );
}

#[test]
fn a_position_independent_executable_is_a_program_and_a_shared_object_is_not() {
    let executable = inspect(&elf64(EM_X86_64, 0, ElfKind::PositionIndependent))
        .expect("a position-independent executable is a program");
    assert_eq!(executable.architecture(), Some(BinaryArchitecture::X86_64));

    assert!(matches!(
        inspect(&elf64(EM_X86_64, 0, ElfKind::Shared)),
        Err(InspectError::NotAnExecutable(_))
    ));
}

#[test]
fn a_pe_library_is_not_an_executable() {
    assert!(matches!(
        inspect(&pe_library(MACHINE_AMD64)),
        Err(InspectError::NotAnExecutable(_))
    ));
}

#[test]
fn a_macho_names_its_format_and_its_platform() {
    let executable =
        inspect(&macho64(CPU_ARM64, Some(object::macho::PLATFORM_MACOS.0))).expect("a Mach-O");
    assert_eq!(executable.format(), BinaryFormat::MachO);
    assert_eq!(executable.architecture(), Some(BinaryArchitecture::Arm64));
    assert_eq!(
        executable.operating_system(),
        Some(TargetOperatingSystem::MacOSX(None))
    );
    assert_eq!(executable.program(), None);
}

#[test]
fn a_macho_without_a_build_version_names_no_operating_system() {
    assert_eq!(
        inspect(&macho64(CPU_X86_64, None))
            .expect("a Mach-O")
            .operating_system(),
        None
    );
}

#[test]
fn a_universal_macho_carries_every_machine_it_contains() {
    let universal = fat_macho64(&[
        macho64(CPU_X86_64, Some(object::macho::PLATFORM_MACOS.0)),
        macho64(CPU_ARM64, Some(object::macho::PLATFORM_MACOS.0)),
    ]);
    let executable = inspect(&universal).expect("a universal binary");
    assert_eq!(executable.format(), BinaryFormat::MachOFat);
    assert_eq!(
        executable.architectures().to_vec(),
        vec![BinaryArchitecture::X86_64, BinaryArchitecture::Arm64]
    );
    assert_eq!(executable.architecture(), None);
    assert!(executable.carries(BinaryArchitecture::Arm64));
    assert!(!executable.carries(BinaryArchitecture::X86_32));
    assert_eq!(
        executable.operating_system(),
        Some(TargetOperatingSystem::MacOSX(None))
    );
}

#[test]
fn a_binary_that_claims_a_format_and_is_not_one_is_malformed() {
    // A valid `PE\0\0` at the offset the DOS header names, and a COFF header whose
    // optional-header size is nonsense.
    let mut truncated = pe(MACHINE_AMD64, SUBSYSTEM_CUI);
    put(&mut truncated, 0x44 + 16, &4u16.to_le_bytes());
    assert!(matches!(
        inspect(&truncated),
        Err(InspectError::Malformed(_))
    ));

    // The same image with the `PE\0\0` signature gone.
    let mut unsigned = pe(MACHINE_AMD64, SUBSYSTEM_CUI);
    put(&mut unsigned, 0x40, &[0; 4]);
    assert!(matches!(
        inspect(&unsigned),
        Err(InspectError::NotAnExecutable(_))
    ));
}

#[test]
fn a_file_that_is_not_an_executable_at_all_is_refused_before_anything_else() {
    for bytes in [
        &b"\0asm\x01\0\0\0"[..],
        &b"<Project Sdk=\"Microsoft.NET.Sdk\">"[..],
        &b"#include <stdio.h>"[..],
        &b"MZ"[..],
        &b""[..],
    ] {
        assert!(
            matches!(inspect(bytes), Err(InspectError::NotAnExecutable(_))),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn a_machine_zup_cannot_name_is_refused_as_an_unsupported_architecture() {
    for bytes in [
        pe(MACHINE_SPARC, SUBSYSTEM_CUI),
        elf64(EM_SPARCV9, 0, ElfKind::Fixed),
        macho64(0x0100_0006, None), // CPU_TYPE_POWERPC64
    ] {
        assert!(
            matches!(
                inspect(&bytes),
                Err(InspectError::UnsupportedArchitecture(_))
            ),
            "a machine with no target spelling is refused as one"
        );
    }
    // A refusal rather than a placeholder: there is no machine value a caller
    // could mistake for something zup knows how to build for.
    assert!(
        BinaryArchitecture::of_target(&target("sparc64-unknown-linux-gnu")).is_none(),
        "a triple zup cannot spell has no machine"
    );
}

#[test]
fn a_format_zup_reads_and_does_not_build_is_named_rather_than_guessed() {
    assert!(matches!(
        inspect(&[0x64, 0x86, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Err(InspectError::NotAnExecutable(name)) if name == "Coff"
    ));
    assert!(matches!(
        inspect(&[0x01, 0xdf, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Err(InspectError::UnsupportedFormat(name)) if name == "Xcoff32"
    ));
}

#[test]
fn a_binary_matches_the_target_it_was_built_for() {
    let amd64 = inspect(&pe(MACHINE_AMD64, SUBSYSTEM_CUI)).expect("a PE");
    assert!(amd64.matches_target(&target("x86_64-pc-windows-msvc")));
    // The ABI and the vendor are not in the file, so neither is compared.
    assert!(amd64.matches_target(&target("x86_64-pc-windows-gnu")));
    assert!(amd64.matches_target(&target("x86_64-unknown-windows-msvc")));
}

#[test]
fn a_target_mismatch_says_which_field_the_file_contradicted() {
    let amd64 = inspect(&pe(MACHINE_AMD64, SUBSYSTEM_CUI)).expect("a PE");

    let refusal = amd64
        .refuse_target(&target("aarch64-pc-windows-msvc"))
        .expect_err("machine");
    assert!(matches!(
        refusal,
        TargetRefusal::Architecture { declared, .. } if matches!(declared, zup_core::TargetArchitecture::Aarch64(_))
    ));
    assert!(
        refusal
            .to_string()
            .contains("declared target does not match binary"),
        "{refusal}"
    );

    // A Linux target contradicts a PE's operating system, and the refusal names
    // that rather than the machine, which agreed.
    let refusal = amd64
        .refuse_target(&target("x86_64-unknown-linux-gnu"))
        .expect_err("system");
    assert!(matches!(
        refusal,
        TargetRefusal::OperatingSystem { found, .. } if found == TargetOperatingSystem::Windows
    ));
}

#[test]
fn a_file_that_names_no_operating_system_constrains_nothing() {
    let elf = inspect(&elf64(EM_X86_64, 0, ElfKind::Fixed)).expect("an ELF");
    assert!(elf.matches_target(&target("x86_64-unknown-linux-gnu")));
    assert!(elf.matches_target(&target("x86_64-unknown-freebsd")));
    assert!(!elf.matches_target(&target("aarch64-unknown-linux-gnu")));

    let macos =
        inspect(&macho64(CPU_ARM64, Some(object::macho::PLATFORM_MACOS.0))).expect("a Mach-O");
    assert!(macos.matches_target(&target("aarch64-apple-darwin")));
    assert!(macos.matches_target(&target("aarch64-apple-macosx")));
    assert!(!macos.matches_target(&target("aarch64-pc-windows-msvc")));
}

#[test]
fn every_arm_profile_a_triple_may_name_is_the_same_machine() {
    let arm = inspect(&elf64(EM_ARM, 0, ElfKind::Fixed)).expect("an ELF");
    for triple in [
        "arm-unknown-linux-gnueabi",
        "armv7-unknown-linux-gnueabihf",
        "thumbv7neon-unknown-linux-gnueabihf",
    ] {
        assert!(
            arm.matches_target(&target(triple)),
            "a 32-bit ARM image is the same machine as {triple}"
        );
    }
}

#[test]
fn a_universal_binary_matches_every_target_it_contains() {
    let universal = inspect(&fat_macho64(&[
        macho64(CPU_X86_64, Some(object::macho::PLATFORM_MACOS.0)),
        macho64(CPU_ARM64, Some(object::macho::PLATFORM_MACOS.0)),
    ]))
    .expect("a universal binary");
    assert!(universal.matches_target(&target("x86_64-apple-darwin")));
    assert!(universal.matches_target(&target("aarch64-apple-darwin")));
    assert!(!universal.matches_target(&target("i686-apple-darwin")));
}

#[test]
fn a_console_program_serves_a_headless_frontend_and_a_window_does_not() {
    let console = inspect(&pe(MACHINE_AMD64, SUBSYSTEM_CUI)).expect("a PE");
    assert!(console.matches_frontend(zup_core::Frontend::Console));
    assert!(console.matches_frontend(zup_core::Frontend::Headless));
    assert!(!console.matches_frontend(zup_core::Frontend::Gui));

    let window = inspect(&pe(MACHINE_AMD64, SUBSYSTEM_GUI)).expect("a PE");
    assert!(window.matches_frontend(zup_core::Frontend::Gui));
    assert!(!window.matches_frontend(zup_core::Frontend::Console));
}

#[test]
fn a_format_with_no_subsystem_field_contradicts_no_frontend() {
    for bytes in [
        macho64(CPU_ARM64, Some(object::macho::PLATFORM_MACOS.0)),
        elf64(EM_AARCH64, 0, ElfKind::Fixed),
    ] {
        let executable = inspect(&bytes).expect("an executable");
        for frontend in [
            zup_core::Frontend::Gui,
            zup_core::Frontend::Console,
            zup_core::Frontend::Headless,
        ] {
            assert!(
                executable.matches_frontend(frontend),
                "{frontend} against a format with no subsystem field"
            );
        }
    }
}

#[test]
fn the_host_architecture_is_one_this_crate_can_name() {
    assert_eq!(
        BinaryArchitecture::host().map(|machine| machine.to_string()),
        BinaryArchitecture::host().map(|machine| match machine {
            BinaryArchitecture::X86_32 => "x86".to_owned(),
            BinaryArchitecture::X86_64 => "x86_64".to_owned(),
            BinaryArchitecture::Arm32 => "arm".to_owned(),
            BinaryArchitecture::Arm64 => "aarch64".to_owned(),
        })
    );
}

#[test]
fn reading_from_a_path_and_from_bytes_agree() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("runtime.exe");
    std::fs::write(&path, pe(MACHINE_ARM64, SUBSYSTEM_GUI)).expect("write");
    let from_path = Executable::read(&path).expect("a readable executable");
    assert_eq!(
        from_path,
        inspect(&pe(MACHINE_ARM64, SUBSYSTEM_GUI)).expect("a PE")
    );
    assert!(matches!(
        Executable::read(&directory.path().join("absent.exe")),
        Err(InspectError::Io(_))
    ));
}
