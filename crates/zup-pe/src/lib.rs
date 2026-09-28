//! Portable Executable headers and resources.
//!
//! This is the native implementation, and it is deliberately small. Two things
//! need it and neither can use a generic object-file rewriter:
//!
//! - **Composition** writes resources into an image that is about to be signed.
//!   Rewriting a PE through a general library would have to reproduce section
//!   alignment, the resource directory, and the certificate table exactly, and
//!   the certificate table is what Authenticode later covers.
//! - **Reading** an image's own resources is how a program finds the artifact it
//!   was built into.
//!
//! A build-time inspector answers a different question - what format and machine
//! an arbitrary template is - and belongs in `zup-build`, which is portable.
//!
//! # What this crate is and is not allowed to know
//!
//! A PE is a file format, and Authenticode is part of it: an image may carry a
//! certificate table holding a PKCS#7 blob, and the digest that blob claims over
//! the image is computed by a rule the format specifies. [`authenticode`] reads
//! and computes both, on every platform, because both are properties of the
//! bytes.
//!
//! Whether *Windows* trusts the chain, whether the publisher matches a project's
//! policy, whether a timestamp is acceptable, and whether a file is safe to run
//! are not properties of the bytes. They are answers from a machine's trust
//! store, and they live in the Windows adapter, which is the only place that can
//! ask. The line is drawn deliberately: this crate will tell you a signature
//! covers these bytes, and it will refuse to tell you anybody should install it.
//!
//! The PE parsing itself is delegated where a maintained implementation exists.
//! [`authenticode`] uses `google/authenticode-rs` for the certificate table, the
//! `WIN_CERTIFICATE` walk and the image digest, and this crate contributes the
//! image layout those routines read: the header is parsed here, once, and
//! everything that needs a section range or a data directory asks for it.
//!
//! The header is read **without loading the file**, because a universal artifact
//! is measured in gigabytes and `is_signed` is asked about one every time
//! anything composes. Only [`authenticode::image_digest`] needs the whole image,
//! and only because the digest rule reads sections in an order the file is not
//! laid out in.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use thiserror::Error;

pub mod authenticode;

pub use authenticode::{
    Certificate, DigestAlgorithm, EmbeddedSignature, SignatureDigest, SigningCertificate,
    authenticode_digest, certificates, embedded_signature, image_digest, signature_blob,
    signing_certificate,
};

/// The resource type every zup container stores its documents under.
pub const RESOURCE_TYPE_RCDATA: u16 = 10;

/// The resource identifier the artifact index occupies.
pub const RESOURCE_ID_INDEX: usize = 1;

/// The first resource identifier after the index, where a container's own
/// document sequence begins.
pub const RESOURCE_ID_BLOB_START: usize = 2;

/// Largest size one resource may have, because a resource length is a 32-bit
/// field in the resource directory.
pub const MAX_RESOURCE_SIZE: u64 = u32::MAX as u64;

/// Largest resource identifier a container may use.
pub const MAX_RESOURCE_ID: usize = u16::MAX as usize - 1;

/// The machine type a PE header names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    I386,
    Amd64,
    Arm64,
    /// A machine type this build does not model.
    Other(u16),
}

impl Machine {
    /// The stable token for this machine type, which is also the architecture
    /// name a target triple uses where they agree.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::I386 => "i686",
            Self::Amd64 => "x86_64",
            Self::Arm64 => "aarch64",
            Self::Other(_) => "other",
        }
    }
}

impl std::fmt::Display for Machine {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(raw) => write!(out, "machine 0x{raw:04x}"),
            machine => out.write_str(machine.as_str()),
        }
    }
}

/// The subsystem a PE header names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Console,
    Gui,
    /// A subsystem this build does not model.
    Other(u16),
}

/// The offsets a PE image's layout is made of.
///
/// Every field is a file offset, which is the thing the format specification is
/// explicit about and the thing a reader gets wrong: the Certificate Table data
/// directory holds a **file offset, not an RVA**, because the table is not
/// mapped into memory and is not part of any section. Everything else in a PE
/// directory is an RVA.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    /// `SizeOfHeaders`: the end of the header region, and the first byte hashed
    /// after the three excluded fields below.
    after_header: usize,
    /// The `CheckSum` field itself, which the Authenticode digest excludes.
    check_sum: usize,
    /// The byte after `CheckSum`.
    after_check_sum: usize,
    /// The Security data directory entry itself, which the digest also excludes:
    /// it names the certificate table, and hashing it would make the digest
    /// depend on the very signature being computed.
    security_data_dir: usize,
    /// The byte after that entry.
    after_security_data_dir: usize,
    /// Each section's raw data range, in the order the section table lists them.
    /// The digest sorts these by start offset before hashing, because the file
    /// order and the load order are not the same order.
    sections: Vec<Range<usize>>,
    /// The certificate table's file range, or `None` when the directory is zeroed.
    certificate_table: Option<Range<usize>>,
}

/// The parts of a PE header zup reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeHeader {
    pub machine: Machine,
    pub subsystem: Subsystem,
    raw_machine: u16,
    raw_subsystem: u16,
    layout: Layout,
}

impl PeHeader {
    /// Whether the image already carries a certificate table.
    ///
    /// A structural fact, read from the data directory and nothing more: it says
    /// the image was *given* a signature, not that the signature is intact. A
    /// table that does not parse is still a table, and composition must refuse
    /// to write resources over either.
    pub fn is_signed(&self) -> bool {
        self.layout.certificate_table.is_some()
    }

    /// The certificate table's file range, if the image has one.
    pub fn certificate_table(&self) -> Option<Range<usize>> {
        self.layout.certificate_table.clone()
    }
}

/// Failures produced by the PE reader.
#[derive(Debug, Error)]
pub enum PeError {
    #[error("PE I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("image is truncated, corrupt, or not a Portable Executable")]
    Invalid,
    #[error("resource data is {size} bytes; the limit is {limit} bytes")]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("container would use {count} resources; the limit is {limit}")]
    TooManyResources { count: usize, limit: usize },
}

/// The smallest optional header zup can read anything from: the `SizeOfHeaders`,
/// `CheckSum` and `Subsystem` fields end at byte 70, and the last four bytes
/// after them are the start of the data directories.
const PE_MIN_OPTIONAL_HEADER_SIZE: u64 = 70;
/// Offsets inside the optional header. These are the same for PE32 and PE32+:
/// the two layouts differ only below `SectionAlignment`, and a PE32+ image
/// replaces `{BaseOfData, ImageBase}` with a 64-bit `ImageBase`, which is
/// exactly as wide, so every field from `SectionAlignment` on lands at the same
/// offset in both.
const PE_SIZE_OF_HEADERS_OFFSET: u64 = 60;
const PE_CHECKSUM_OFFSET: u64 = 64;
const PE_SUBSYSTEM_OFFSET: u64 = 68;
/// The Certificate Table is data directory entry 4.
const PE_SECURITY_DIRECTORY_INDEX: u64 = 4;
const PE_DATA_DIRECTORY_SIZE: u64 = 8;
const PE_SECTION_HEADER_SIZE: u64 = 40;
const PE_SECTION_POINTER_TO_RAW_DATA: u64 = 20;
const PE_SECTION_SIZE_OF_RAW_DATA: u64 = 16;
const IMAGE_SUBSYSTEM_GUI: u16 = 2;
const IMAGE_SUBSYSTEM_CUI: u16 = 3;

/// Read the header fields zup needs from an image.
///
/// Seeks rather than slurps: the header is a few hundred bytes at a fixed set of
/// offsets, and the only reason to read more of the file is a caller that asks
/// [`PeHeader::read_resource`] for one.
pub fn read_pe_header(path: &Path) -> Result<PeHeader, PeError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    read_header(&mut file, len)
}

fn read_header(source: &mut (impl Read + Seek), len: u64) -> Result<PeHeader, PeError> {
    let file = source;
    if len < 0x40 {
        return Err(PeError::Invalid);
    }
    let mut dos = [0u8; 0x40];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(PeError::Invalid);
    }
    let pe = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if pe < 0x40 || pe.checked_add(24).is_none_or(|end| end > len) {
        return Err(PeError::Invalid);
    }
    file.seek(SeekFrom::Start(pe))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(PeError::Invalid);
    }
    let mut coff = [0u8; 20];
    file.read_exact(&mut coff)?;
    let raw_machine = u16::from_le_bytes(coff[..2].try_into().unwrap());
    let section_count = u16::from_le_bytes(coff[2..4].try_into().unwrap());
    let optional_offset = pe + 24;
    let optional_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let optional_end = optional_offset
        .checked_add(optional_len)
        .ok_or(PeError::Invalid)?;
    if optional_len < PE_MIN_OPTIONAL_HEADER_SIZE || optional_end > len || section_count == 0 {
        return Err(PeError::Invalid);
    }
    let mut optional = vec![0u8; optional_len as usize];
    file.seek(SeekFrom::Start(optional_offset))?;
    file.read_exact(&mut optional)?;
    let data_directory_offset: u64 = match u16::from_le_bytes(optional[..2].try_into().unwrap()) {
        0x10b => 96,
        0x20b => 112,
        _ => return Err(PeError::Invalid),
    };
    let field = |at: u64| -> Result<u32, PeError> {
        let end = at.checked_add(4).ok_or(PeError::Invalid)?;
        let bytes = optional
            .get(at as usize..end as usize)
            .ok_or(PeError::Invalid)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    };
    let security_offset = data_directory_offset
        .checked_add(PE_SECURITY_DIRECTORY_INDEX * PE_DATA_DIRECTORY_SIZE)
        .ok_or(PeError::Invalid)?;
    if security_offset
        .checked_add(2 * PE_DATA_DIRECTORY_SIZE)
        .is_none_or(|end| end > optional_end)
    {
        return Err(PeError::Invalid);
    }
    let check_sum = optional_offset + PE_CHECKSUM_OFFSET;
    let security_data_dir = optional_offset + security_offset;
    let raw_subsystem = u16::from_le_bytes(
        optional[PE_SUBSYSTEM_OFFSET as usize..PE_SUBSYSTEM_OFFSET as usize + 2]
            .try_into()
            .unwrap(),
    );

    // The certificate table's directory entry is `{ u32 address; u32 size }` - the
    // size sits 4 bytes after the address, because both are 32-bit fields even
    // though the *stride* between directory entries is 8. And the address is a
    // file offset, not an RVA: the table is not mapped into memory and is not
    // part of any section. A zeroed entry is how an unsigned image says so.
    let certificate_address = field(security_offset)? as u64;
    let certificate_size = field(security_offset + 4)? as u64;

    let certificate_table = if certificate_address == 0 || certificate_size == 0 {
        None
    } else {
        let end = certificate_address
            .checked_add(certificate_size)
            .ok_or(PeError::Invalid)?;
        Some(certificate_address as usize..end as usize)
    };

    let section_table = optional_end;
    let section_end = section_table
        .checked_add(
            u64::from(section_count)
                .checked_mul(PE_SECTION_HEADER_SIZE)
                .ok_or(PeError::Invalid)?,
        )
        .ok_or(PeError::Invalid)?;
    if section_end > len {
        return Err(PeError::Invalid);
    }
    let mut sections = Vec::with_capacity(usize::from(section_count));
    for index in 0..u64::from(section_count) {
        let at = section_table + index * PE_SECTION_HEADER_SIZE;
        let mut header = [0u8; PE_SECTION_HEADER_SIZE as usize];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut header)?;
        let size = u32::from_le_bytes(
            header[PE_SECTION_SIZE_OF_RAW_DATA as usize..PE_SECTION_SIZE_OF_RAW_DATA as usize + 4]
                .try_into()
                .unwrap(),
        ) as u64;
        let start = u32::from_le_bytes(
            header[PE_SECTION_POINTER_TO_RAW_DATA as usize
                ..PE_SECTION_POINTER_TO_RAW_DATA as usize + 4]
                .try_into()
                .unwrap(),
        ) as u64;
        // A section with no raw bytes is a section that is not in the file, and
        // a raw range that runs past the end is a truncated image. Both are
        // refused here so the digest has ranges it can hash.
        let end = start.checked_add(size).ok_or(PeError::Invalid)?;
        if end > len {
            return Err(PeError::Invalid);
        }
        sections.push(start as usize..end as usize);
    }

    Ok(PeHeader {
        machine: match raw_machine {
            0x014c => Machine::I386,
            0x8664 => Machine::Amd64,
            0xaa64 => Machine::Arm64,
            other => Machine::Other(other),
        },
        subsystem: match raw_subsystem {
            IMAGE_SUBSYSTEM_CUI => Subsystem::Console,
            IMAGE_SUBSYSTEM_GUI => Subsystem::Gui,
            other => Subsystem::Other(other),
        },
        raw_machine,
        raw_subsystem,
        layout: Layout {
            after_header: field(PE_SIZE_OF_HEADERS_OFFSET)? as usize,
            check_sum: check_sum as usize,
            after_check_sum: (check_sum + 4) as usize,
            security_data_dir: security_data_dir as usize,
            // Exactly one directory entry is skipped, not the rest of the
            // directory: the specification excludes the Certificate Table entry
            // and resumes at the next byte, so the header after it is hashed.
            after_security_data_dir: (security_data_dir + PE_DATA_DIRECTORY_SIZE) as usize,
            sections,
            certificate_table,
        },
    })
}

/// The raw machine value, for a caller that maps it to its own vocabulary.
pub const fn raw_machine(header: &PeHeader) -> u16 {
    header.raw_machine
}

/// The raw subsystem value.
pub const fn raw_subsystem(header: &PeHeader) -> u16 {
    header.raw_subsystem
}

/// Whether an image's certificate table directory is populated, which is what
/// composition must refuse to write over.
///
/// Seeks, reads eight bytes, and answers. A universal artifact is measured in
/// gigabytes and this is asked about one every time anything is composed, so it
/// cannot be a question that loads the file.
pub fn is_signed(path: &Path) -> Result<bool, PeError> {
    Ok(read_pe_header(path)?.is_signed())
}

/// Whether an image starts with a DOS signature.
pub fn looks_like_pe(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && &magic == b"MZ"
}

/// One document a container writes, addressed by its resource identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDocument {
    pub id: usize,
    pub bytes: Vec<u8>,
}

/// Check that a document set is writable: identifiers start where the caller
/// says, do not collide, and fit both the resource length and identifier bounds.
pub fn check_documents(documents: &[ResourceDocument], first_id: usize) -> Result<(), PeError> {
    if documents.iter().any(|document| {
        document.id > MAX_RESOURCE_ID || document.bytes.len() as u64 > MAX_RESOURCE_SIZE
    }) {
        return Err(PeError::TooManyResources {
            count: documents.len(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for document in documents {
        if document.id < first_id || !seen.insert(document.id) {
            return Err(PeError::TooManyResources {
                count: documents.len(),
                limit: MAX_RESOURCE_ID,
            });
        }
    }
    Ok(())
}

/// A whole image in memory, and the layout needed to hash it.
///
/// The digest rule reads the header, then the sections in ascending file order,
/// then the remainder of the file up to the certificate table. That is three
/// disjoint regions, and the only way to hand them to a routine that takes one
/// slice is to have the file. So this type is the price of the digest, and it is
/// only built by callers that asked for the digest.
pub struct Image {
    bytes: Vec<u8>,
    header: PeHeader,
}

impl Image {
    /// Load an image.
    ///
    /// The whole file. A universal artifact is measured in gigabytes, so a
    /// caller that only needs to know *whether* a certificate table exists
    /// should use [`read_pe_header`] or [`is_signed`] instead.
    pub fn read(path: &Path) -> Result<Self, PeError> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        Self::from_bytes(bytes)
    }

    /// Interpret bytes already in memory as an image.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, PeError> {
        let len = bytes.len() as u64;
        let header = read_header(&mut std::io::Cursor::new(bytes.as_slice()), len)?;
        Ok(Self { bytes, header })
    }

    /// The image's header.
    pub fn header(&self) -> &PeHeader {
        &self.header
    }

    /// The image's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl authenticode::PeTrait for Image {
    fn data(&self) -> &[u8] {
        &self.bytes
    }

    fn num_sections(&self) -> usize {
        self.header.layout.sections.len()
    }

    fn section_data_range(
        &self,
        index: usize,
    ) -> Result<Range<usize>, authenticode::PeOffsetError> {
        // The `authenticode` trait indexes sections from 1, matching the
        // specification's numbering; the layout is a 0-based vector.
        self.header
            .layout
            .sections
            .get(index.checked_sub(1).ok_or(authenticode::PeOffsetError)?)
            .cloned()
            .ok_or(authenticode::PeOffsetError)
    }

    fn certificate_table_range(&self) -> Result<Option<Range<usize>>, authenticode::PeOffsetError> {
        Ok(self.header.layout.certificate_table.clone())
    }

    fn offsets(&self) -> Result<authenticode::PeOffsets, authenticode::PeOffsetError> {
        let layout = &self.header.layout;
        if layout.after_header > self.bytes.len()
            || layout.check_sum > layout.after_check_sum
            || layout.security_data_dir > layout.after_security_data_dir
        {
            return Err(authenticode::PeOffsetError);
        }
        Ok(authenticode::PeOffsets {
            check_sum: layout.check_sum,
            after_check_sum: layout.after_check_sum,
            security_data_dir: layout.security_data_dir,
            after_security_data_dir: layout.after_security_data_dir,
            after_header: layout.after_header,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(subsystem: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x170];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
        bytes
    }

    fn write(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (directory, path)
    }

    #[test]
    fn machine_and_subsystem_are_read_from_the_header() {
        let (_dir, path) = write(&image(3), "cui.exe");
        let header = read_pe_header(&path).unwrap();
        assert_eq!(header.machine, Machine::Amd64);
        assert_eq!(header.subsystem, Subsystem::Console);

        let (_dir, path) = write(&image(2), "gui.exe");
        assert_eq!(read_pe_header(&path).unwrap().subsystem, Subsystem::Gui);

        let (_dir, path) = write(&image(9), "odd.exe");
        assert_eq!(
            read_pe_header(&path).unwrap().subsystem,
            Subsystem::Other(9)
        );
    }

    #[test]
    fn an_unsigned_image_reports_itself_as_unsigned() {
        let (_dir, path) = write(&image(2), "gui.exe");
        assert!(!is_signed(&path).unwrap());
        assert!(!read_pe_header(&path).unwrap().is_signed());
        assert_eq!(read_pe_header(&path).unwrap().certificate_table(), None);
    }

    /// The Certificate Table data directory is the one directory whose address
    /// is a **file offset** rather than an RVA, because the table is not mapped
    /// into memory. Reading it as an RVA would look for the signature somewhere
    /// inside a section and find nothing.
    ///
    #[test]
    fn a_truncated_or_foreign_file_is_rejected() {
        let (_dir, path) = write(b"not an image", "x.bin");
        assert!(matches!(read_pe_header(&path), Err(PeError::Invalid)));
        assert!(!looks_like_pe(&path));

        let (_dir, path) = write(&image(2)[..0x50], "short.exe");
        assert!(matches!(read_pe_header(&path), Err(PeError::Invalid)));
    }

    /// A section header that claims raw bytes past the end of the file is a
    /// truncated image, and the digest has ranges it can hash or it has nothing.
    #[test]
    fn a_section_that_runs_past_the_end_of_the_file_is_rejected() {
        const SECTION: usize = 0x58 + 240;
        let mut bytes = image(2);
        bytes[SECTION + 20..SECTION + 24].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[SECTION + 16..SECTION + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        let (_dir, path) = write(&bytes, "overrun.exe");
        assert!(matches!(read_pe_header(&path), Err(PeError::Invalid)));
    }

    #[test]
    fn document_identifiers_must_be_unique_and_within_bounds() {
        let ok = vec![ResourceDocument {
            id: RESOURCE_ID_BLOB_START,
            bytes: b"a".to_vec(),
        }];
        assert!(check_documents(&ok, RESOURCE_ID_INDEX).is_ok());
        assert!(
            check_documents(&ok, RESOURCE_ID_BLOB_START).is_ok(),
            "the index and the blob sequence share one identifier space"
        );
        assert!(
            check_documents(&ok, RESOURCE_ID_BLOB_START + 1).is_err(),
            "a document below the first writable identifier is refused"
        );
        let duplicated = vec![
            ResourceDocument {
                id: 4,
                bytes: b"a".to_vec(),
            },
            ResourceDocument {
                id: 4,
                bytes: b"b".to_vec(),
            },
        ];
        assert!(check_documents(&duplicated, RESOURCE_ID_BLOB_START).is_err());
    }
}
