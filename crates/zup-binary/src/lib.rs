//! Portable, read-only inspection of executable files.
//!
//! `object` already answers what format a file is, what machine it is for, and
//! what it records about itself, for PE/COFF, ELF and Mach-O. This crate is the
//! one place that asks, and the only place that translates the answer into zup's
//! vocabulary. Deciding whether the answer is *acceptable* is the caller's: a
//! target triple is a claim a manifest makes, and this crate only reports where
//! the file contradicts one.
//!
//! # What a file can prove
//!
//! Architecture always. An operating system only when the format records one: a
//! PE is a Windows image, an ELF says so when its OSABI is not the System V
//! default, and a Mach-O only from `LC_BUILD_VERSION`. No format records a vendor
//! or an ABI, so [`refuse_target`](crate::Executable::refuse_target) never
//! compares one.
//!
//! This crate reads. Writing a PE that is about to be signed has to reproduce
//! section alignment, the resource directory and the certificate table exactly,
//! which is `BeginUpdateResourceW`'s job - see `zup-pe`.

mod target;
#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Deref;
use std::path::Path;

use object::read::elf::ProgramHeader;
use object::read::macho::{FatArch, MachOFatFile};
use object::read::pe::{ImageNtHeaders, ImageOptionalHeader};
use object::read::{Object, ObjectKind};
use object::{Architecture, Endian, Endianness, File, FileKind};
use thiserror::Error;
use zup_core::{TargetArchitecture, TargetOperatingSystem, TargetTriple};

pub use target::{FrontendRefusal, TargetRefusal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryFormat {
    /// A Windows image: a DOS stub, a PE header, and a section table.
    Pe,
    Elf,
    MachO,
    /// A Mach-O universal binary: a fat header over several Mach-O images.
    MachOFat,
}

impl fmt::Display for BinaryFormat {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Pe => "PE",
            Self::Elf => "ELF",
            Self::MachO => "Mach-O",
            Self::MachOFat => "Mach-O universal",
        })
    }
}

/// A CPU zup has a target spelling for. Anything else is an
/// [`InspectError::UnsupportedArchitecture`]: a machine zup cannot name is a
/// machine zup cannot target, and an `Other(u32)` would let that state travel as
/// a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryArchitecture {
    /// Intel 80386 and its successors in 32-bit mode.
    X86_32,
    X86_64,
    /// 32-bit ARM, whatever instruction set profile the triple names: a file
    /// cannot tell `armv7` from `thumbv7neon` either.
    Arm32,
    Arm64,
}

impl BinaryArchitecture {
    /// The architecture of the CPU this process runs on.
    pub fn host() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86" => Some(Self::X86_32),
            "x86_64" => Some(Self::X86_64),
            "arm" => Some(Self::Arm32),
            "aarch64" => Some(Self::Arm64),
            _ => None,
        }
    }

    /// The architecture a target triple names, or `None` when zup has no spelling.
    pub fn of_target(target: &TargetTriple) -> Option<Self> {
        match target.architecture() {
            TargetArchitecture::X86_32(_) => Some(Self::X86_32),
            TargetArchitecture::X86_64 => Some(Self::X86_64),
            TargetArchitecture::Arm(_) => Some(Self::Arm32),
            TargetArchitecture::Aarch64(_) => Some(Self::Arm64),
            _ => None,
        }
    }

    const fn of_object(architecture: Architecture) -> Option<Self> {
        match architecture {
            Architecture::I386 => Some(Self::X86_32),
            Architecture::X86_64 => Some(Self::X86_64),
            Architecture::Arm => Some(Self::Arm32),
            Architecture::Aarch64 => Some(Self::Arm64),
            _ => None,
        }
    }
}

impl fmt::Display for BinaryArchitecture {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::X86_32 => "x86",
            Self::X86_64 => "x86_64",
            Self::Arm32 => "arm",
            Self::Arm64 => "aarch64",
        })
    }
}

/// The architectures one executable names, non-empty by construction: a universal
/// Mach-O carries one image per machine and a thin image carries exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Architectures(Vec<BinaryArchitecture>);

impl Architectures {
    pub fn single(&self) -> Option<BinaryArchitecture> {
        match self.0.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }
}

impl Deref for Architectures {
    type Target = [BinaryArchitecture];

    fn deref(&self) -> &[BinaryArchitecture] {
        &self.0
    }
}

impl fmt::Display for Architectures {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, architecture) in self.0.iter().enumerate() {
            if index > 0 {
                out.write_str(if index + 1 == self.0.len() {
                    " and "
                } else {
                    ", "
                })?;
            }
            write!(out, "{architecture}")?;
        }
        Ok(())
    }
}

/// Whether an executable presents a window or a terminal. Both come from one PE
/// `Subsystem` field; `None` is a format that records neither, which is why it is
/// not a third variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramKind {
    Windowed,
    Console,
}

impl fmt::Display for ProgramKind {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Windowed => "windowed",
            Self::Console => "console",
        })
    }
}

#[derive(Debug, Error)]
pub enum InspectError {
    #[error("cannot read the executable: {0}")]
    Io(#[from] std::io::Error),
    /// The file does not begin with the magic of any executable zup reads, or
    /// carries no program image. A package, a script, a shared library and a text
    /// file all land here, and so does a file too short to hold a magic.
    #[error("not a supported executable: {0}")]
    NotAnExecutable(String),
    /// An executable format zup parses and does not build, run, or target.
    #[error("unsupported binary format: {0}")]
    UnsupportedFormat(String),
    #[error("unsupported architecture: {0}")]
    UnsupportedArchitecture(String),
    /// The file announces an executable format and is not one: a truncated image,
    /// a section table past the end, a load command whose size is nonsense.
    #[error("malformed executable: {0}")]
    Malformed(String),
}

/// One executable file, read and described. Every question is total, because a
/// file that could not answer was refused by [`Executable::read`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    format: BinaryFormat,
    architectures: Architectures,
    program: Option<ProgramKind>,
    operating_system: Option<TargetOperatingSystem>,
}

impl Executable {
    /// Read the executable at `path`.
    pub fn read(path: &Path) -> Result<Self, InspectError> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, InspectError> {
        let (format, images) = read_images(bytes)?;
        // A universal binary is one program in several images, so the first member
        // that states anything answers for all of them.
        let mut architectures: Vec<BinaryArchitecture> = Vec::with_capacity(images.len());
        let mut program = None;
        let mut operating_system = None;
        for image in images {
            program = program.or(image.program);
            operating_system = operating_system.or(image.operating_system);
            if !architectures.contains(&image.architecture) {
                architectures.push(image.architecture);
            }
        }
        if architectures.is_empty() {
            return Err(malformed(
                "the file announces an executable format and carries no image",
            ));
        }
        Ok(Self {
            format,
            architectures: Architectures(architectures),
            program,
            operating_system,
        })
    }

    pub fn format(&self) -> BinaryFormat {
        self.format
    }

    pub fn architectures(&self) -> &Architectures {
        &self.architectures
    }

    pub fn architecture(&self) -> Option<BinaryArchitecture> {
        self.architectures.single()
    }

    /// Whether this file carries an image for `architecture` - the question a
    /// dispatcher asks, which is not the same as asking which machine it is.
    pub fn carries(&self, architecture: BinaryArchitecture) -> bool {
        self.architectures.contains(&architecture)
    }

    /// `None` means the format has no window/terminal field, not that the program
    /// is a console.
    pub fn program(&self) -> Option<ProgramKind> {
        self.program
    }

    pub fn operating_system(&self) -> Option<TargetOperatingSystem> {
        self.operating_system
    }
}

struct Image {
    architecture: BinaryArchitecture,
    program: Option<ProgramKind>,
    operating_system: Option<TargetOperatingSystem>,
}

fn read_images(bytes: &[u8]) -> Result<(BinaryFormat, Vec<Image>), InspectError> {
    // An unrecognised magic is a file that is not an executable rather than a
    // malformed one; `Malformed` is for a file whose own magic named a format and
    // whose body then failed to be that format.
    let kind =
        FileKind::parse(bytes).map_err(|error| InspectError::NotAnExecutable(error.to_string()))?;
    Ok(match kind {
        FileKind::Pe32 | FileKind::Pe64 => (BinaryFormat::Pe, vec![pe_image(bytes)?]),
        FileKind::Elf32 | FileKind::Elf64 => (BinaryFormat::Elf, vec![elf_image(bytes)?]),
        FileKind::MachO32 | FileKind::MachO64 => (BinaryFormat::MachO, vec![macho_image(bytes)?]),
        FileKind::MachOFat32 => fat_images::<object::macho::FatArch32>(bytes)?,
        FileKind::MachOFat64 => fat_images::<object::macho::FatArch64>(bytes)?,
        other => return Err(classify(other)),
    })
}

/// An archive, a relocatable object and an import library are linker inputs rather
/// than programs, so they are not-an-executable; everything else `object` knows is
/// a program in a shape zup does not build.
fn classify(kind: FileKind) -> InspectError {
    let name = format!("{kind:?}");
    match kind {
        FileKind::Archive | FileKind::Coff | FileKind::CoffBig | FileKind::CoffImport => {
            InspectError::NotAnExecutable(name)
        }
        _ => InspectError::UnsupportedFormat(name),
    }
}

fn fat_images<'data, Fat: FatArch>(
    bytes: &'data [u8],
) -> Result<(BinaryFormat, Vec<Image>), InspectError> {
    let fat = MachOFatFile::<'data, Fat>::parse(bytes).map_err(malformed)?;
    let arches = fat.arches();
    let mut images = Vec::with_capacity(arches.len());
    for arch in arches {
        images.push(macho_image(arch.data(bytes).map_err(malformed)?)?);
    }
    Ok((BinaryFormat::MachOFat, images))
}

fn pe_image(bytes: &[u8]) -> Result<Image, InspectError> {
    let file = File::parse(bytes).map_err(malformed)?;
    if file.kind() != ObjectKind::Executable {
        return Err(not_a_program(file.kind()));
    }
    let machine = machine(&file)?;
    let subsystem = match &file {
        File::Pe32(image) => image.nt_headers().optional_header().subsystem(),
        File::Pe64(image) => image.nt_headers().optional_header().subsystem(),
        _ => return Err(malformed("the file announces a PE and is not one")),
    };
    Ok(Image {
        architecture: machine,
        program: match subsystem {
            object::pe::IMAGE_SUBSYSTEM_WINDOWS_GUI => Some(ProgramKind::Windowed),
            object::pe::IMAGE_SUBSYSTEM_WINDOWS_CUI => Some(ProgramKind::Console),
            _ => None,
        },
        operating_system: Some(TargetOperatingSystem::Windows),
    })
}

fn elf_image(bytes: &[u8]) -> Result<Image, InspectError> {
    let file = File::parse(bytes).map_err(malformed)?;
    if !elf_is_program(&file) {
        return Err(not_a_program(file.kind()));
    }
    let machine = machine(&file)?;
    let os_abi = match &file {
        File::Elf32(image) => image.elf_header().e_ident.os_abi,
        File::Elf64(image) => image.elf_header().e_ident.os_abi,
        _ => return Err(malformed("the file announces an ELF and is not one")),
    };
    Ok(Image {
        architecture: machine,
        program: None,
        operating_system: match os_abi {
            object::elf::ELFOSABI_LINUX => Some(TargetOperatingSystem::Linux),
            object::elf::ELFOSABI_FREEBSD => Some(TargetOperatingSystem::Freebsd),
            object::elf::ELFOSABI_NETBSD => Some(TargetOperatingSystem::Netbsd),
            object::elf::ELFOSABI_OPENBSD => Some(TargetOperatingSystem::Openbsd),
            object::elf::ELFOSABI_SOLARIS => Some(TargetOperatingSystem::Solaris),
            object::elf::ELFOSABI_HURD => Some(TargetOperatingSystem::Hurd),
            // `ELFOSABI_NONE` is System V's "unspecified", which is what most
            // toolchains emit, so it says nothing and the architecture stands
            // alone.
            _ => None,
        },
    })
}

fn macho_image(bytes: &[u8]) -> Result<Image, InspectError> {
    let file = File::parse(bytes).map_err(malformed)?;
    if file.kind() != ObjectKind::Executable {
        return Err(not_a_program(file.kind()));
    }
    let machine = machine(&file)?;
    let operating_system = match &file {
        File::MachO32(image) => image
            .build_version()
            .map_err(malformed)?
            .and_then(|(command, _)| apple_platform(image.endian(), command)),
        File::MachO64(image) => image
            .build_version()
            .map_err(malformed)?
            .and_then(|(command, _)| apple_platform(image.endian(), command)),
        _ => return Err(malformed("the file announces a Mach-O and is not one")),
    };
    Ok(Image {
        architecture: machine,
        program: None,
        operating_system,
    })
}

fn apple_platform<End: Endian>(
    endian: End,
    command: &object::macho::BuildVersionCommand<End>,
) -> Option<TargetOperatingSystem> {
    match command.platform.get(endian) {
        object::macho::PLATFORM_MACOS => Some(TargetOperatingSystem::MacOSX(None)),
        object::macho::PLATFORM_IOS | object::macho::PLATFORM_IOSSIMULATOR => {
            Some(TargetOperatingSystem::IOS(None))
        }
        object::macho::PLATFORM_TVOS | object::macho::PLATFORM_TVOSSIMULATOR => {
            Some(TargetOperatingSystem::TvOS(None))
        }
        object::macho::PLATFORM_WATCHOS | object::macho::PLATFORM_WATCHOSSIMULATOR => {
            Some(TargetOperatingSystem::WatchOS(None))
        }
        object::macho::PLATFORM_VISIONOS | object::macho::PLATFORM_VISIONOSSIMULATOR => {
            Some(TargetOperatingSystem::VisionOS(None))
        }
        _ => None,
    }
}

/// A position-independent executable and a shared object are both `ET_DYN`, and
/// every distribution builds its executables as the first. A `PT_INTERP` program
/// header is what separates them, so treating `ET_DYN` as a library would refuse
/// every real Linux binary.
///
/// The headers are read rather than through `Object::segments`, which yields only
/// the loadable segments.
fn elf_is_program<'data>(file: &File<'data, &'data [u8]>) -> bool {
    match file.kind() {
        ObjectKind::Executable => true,
        ObjectKind::Dynamic => match file {
            File::Elf32(image) => asks_for_a_loader(image.elf_program_headers()),
            File::Elf64(image) => asks_for_a_loader(image.elf_program_headers()),
            _ => false,
        },
        _ => false,
    }
}

fn asks_for_a_loader<Header: ProgramHeader<Endian = Endianness>>(headers: &[Header]) -> bool {
    headers
        .iter()
        .any(|header| header.p_type(Endianness::Little) == object::elf::PT_INTERP)
}

fn machine<'data>(file: &File<'data, &'data [u8]>) -> Result<BinaryArchitecture, InspectError> {
    BinaryArchitecture::of_object(file.architecture())
        .ok_or_else(|| InspectError::UnsupportedArchitecture(format!("{:?}", file.architecture())))
}

fn not_a_program(kind: ObjectKind) -> InspectError {
    InspectError::NotAnExecutable(match kind {
        ObjectKind::Dynamic => "a shared library".to_owned(),
        ObjectKind::Relocatable => "a relocatable object".to_owned(),
        ObjectKind::Core => "a core dump".to_owned(),
        other => format!("{other:?}"),
    })
}

fn malformed(reason: impl fmt::Display) -> InspectError {
    InspectError::Malformed(reason.to_string())
}
