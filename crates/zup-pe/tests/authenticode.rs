//! The Authenticode structure, against real signed images.
//!
//! Every fixture in `fixtures/` is a genuine PE image produced by the Rust
//! toolchain and signed by `Set-AuthenticodeSignature` with a throwaway
//! self-signed certificate. That matters: a fixture built by hand proves only
//! that the parser agrees with the fixture's author, whereas a fixture a *real
//! signer* produced proves the parser agrees with the signing ecosystem — the
//! certificate table's offset, the entry's declared length, the padding, and the
//! digest the signature claims over the image.
//!
//! The fixtures are small because they are a `no_std` binary with a panic
//! handler and an entry point that spins, stripped. A 1.5 KiB image is enough to
//! exercise the whole format and is small enough to read.
//!
//! What is *not* asserted here is anything about trust. The certificate is
//! self-signed and therefore untrusted, which is the correct subject for
//! `zup-windows::signing`'s tests and not this crate's business: this crate can
//! only say whether the digest matches.

use std::path::{Path, PathBuf};

/// The offsets a test needs, read from the fixture rather than hard-coded.
///
/// A test that hard-codes `0x58` because it remembered the layout is a test that
/// fails when the image is a PE32+ rather than a PE32, and one that fails for a
/// reason that has nothing to do with the property under test.
struct Layout {
    optional: usize,
    data_directory: usize,
    section: usize,
    section_count: usize,
    certificate_table: usize,
}

fn layout(bytes: &[u8]) -> Layout {
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    let optional = pe + 24;
    // The COFF file header starts at `pe + 4`; its section count and its optional
    // header size are at fixed offsets within it.
    let section_count = u16::from_le_bytes(bytes[pe + 6..pe + 8].try_into().unwrap()) as usize;
    let optional_len = u16::from_le_bytes(bytes[pe + 20..pe + 22].try_into().unwrap()) as usize;
    let magic = u16::from_le_bytes(bytes[optional..optional + 2].try_into().unwrap());
    let data_directory = optional + if magic == 0x20b { 112 } else { 96 };
    let security = data_directory + 4 * 8;
    Layout {
        optional,
        data_directory,
        section: optional + optional_len,
        section_count,
        certificate_table: u32::from_le_bytes(bytes[security..security + 4].try_into().unwrap())
            as usize,
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// A SHA-256 Authenticode signature covers these bytes.
///
/// The two facts are separate and both are asserted: the structure is there, and
/// the digest the signature claims is the digest of the file. A parser that
/// reported the first and got the second wrong would let a tampered file pass a
/// check that reads as a verification.
#[test]
fn a_signed_image_carries_a_digest_that_matches_its_bytes() {
    for name in ["signed.exe", "timestamped.exe"] {
        let path = fixture(name);
        let signature = zup_pe::embedded_signature(&path)
            .expect("a real signed image parses")
            .unwrap_or_else(|| panic!("{name} carries a signature"));
        assert_eq!(
            signature.digest.algorithm,
            zup_pe::DigestAlgorithm::Sha256,
            "{name} was signed with SHA256"
        );
        assert_eq!(
            signature.digest.value.len(),
            32,
            "{name}: a SHA-256 is 32 bytes"
        );

        let certificate = &signature.certificates[0];
        assert_eq!(certificate.revision, 0x0200, "{name}: current revision");
        assert_eq!(
            certificate.certificate_type, 0x0002,
            "{name}: PKCS#7 SignedData"
        );
        assert!(certificate.len > 100, "{name}: an entry holds a signature");
        assert!(certificate.is_signed_data());

        let digest = zup_pe::image_digest(&path).expect("the image digest computes");
        assert!(
            signature.digest.matches(&digest),
            "{name}: the embedded digest does not match the image, so either the \
             image digest rule or the fixture is wrong"
        );
    }
}

/// The image digest excludes the checksum, the certificate table's data directory
/// entry, and the certificate table itself.
///
/// This is the property that makes signing possible at all, and only a *signed*
/// fixture can show it: `signed.exe` has a non-zero checksum, a populated
/// directory entry and 1456 bytes of certificate table, and its digest is still
/// the digest of the image. Zeroing the two excluded fields must not move the
/// digest; if it does, a file could not be signed without invalidating the
/// digest the signature was computed over.
#[test]
fn the_image_digest_excludes_the_checksum_and_the_certificate_directory() {
    let path = fixture("signed.exe");
    let before = zup_pe::image_digest(&path).expect("digest");
    let bytes = std::fs::read(&path).expect("read");
    let layout = layout(&bytes);
    assert!(
        zup_pe::read_pe_header(&path).expect("header").is_signed(),
        "the fixture has a certificate table"
    );
    assert!(
        bytes[layout.optional + 64..layout.optional + 68]
            .iter()
            .any(|byte| *byte != 0),
        "a signer leaves a non-zero checksum, so the exclusion is actually exercised"
    );

    let after_checksum = {
        let mut altered = bytes.clone();
        altered[layout.optional + 64..layout.optional + 68].copy_from_slice(&0u32.to_le_bytes());
        digest_of(altered)
    };
    assert_eq!(
        before, after_checksum,
        "the checksum is excluded from the image digest, or a file could not be signed"
    );
    // The certificate table is excluded too, and
    // `signing_does_not_change_the_image_digest` is the honest test of that: the
    // same image, signed and unsigned, has one digest.
    //
    // Zeroing the directory entry here would be the obvious test and it is
    // wrong. An entry reading "no table" tells the rule there is nothing to
    // subtract, so the table's bytes would be hashed and the digest would move —
    // for a reason that has nothing to do with the exclusion under test.
    // Relocating the table is no better: whatever the table used to occupy
    // becomes covered bytes, so the digest moves for the same reason.
}

fn digest_of(bytes: Vec<u8>) -> zup_core::Sha256Digest {
    zup_pe::Image::from_bytes(bytes)
        .expect("the altered image still parses")
        .authenticode_digest()
        .expect("digest")
}

/// The certificate table is not in the digest either, which is why the *same*
/// image signed and unsigned has the same image digest.
///
/// That is not a curiosity: it is the property that lets a signature cover an
/// image and lets the signed file be a byte-for-byte superset of the unsigned
/// one. Asserted here so that a future change to the digest rule cannot quietly
/// make signing impossible.
#[test]
fn signing_does_not_change_the_image_digest() {
    let signed = fixture("signed.exe");
    let unsigned = fixture("unsigned.exe");
    assert!(
        !zup_pe::read_pe_header(&unsigned)
            .expect("header")
            .is_signed()
    );
    assert_eq!(
        zup_pe::image_digest(&signed).expect("digest"),
        zup_pe::image_digest(&unsigned).expect("digest"),
        "the certificate table is excluded from the image digest, so the signed and \
         unsigned forms of one image have the same image digest"
    );
}

/// The certificate table's directory address is a **file offset**, not an RVA.
///
/// This is the invariant a reader gets wrong, and the fixture is the only place
/// it can be shown: the table starts past the end of every section, at 1536,
/// where an implementation that treated the address as an RVA and looked inside a
/// mapped section would find nothing.
#[test]
fn the_certificate_table_lives_at_a_file_offset() {
    let path = fixture("signed.exe");
    let header = zup_pe::read_pe_header(&path).expect("header");
    let range = header.certificate_table().expect("a table");
    assert!(header.is_signed());
    assert_eq!(
        range.end,
        std::fs::metadata(&path).expect("metadata").len() as usize,
        "a real signer appends the table at the end of the file"
    );
    let bytes = std::fs::read(&path).expect("read");
    let layout = layout(&bytes);
    let last_end = (0..layout.section_count)
        .map(|index| {
            let header = &bytes[layout.section + index * 40..][..40];
            let raw = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
            let start = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
            start + raw
        })
        .max()
        .expect("at least one section");
    assert!(
        range.start >= last_end,
        "the table is past every section, which is why its address cannot be an RVA"
    );

    let entries = zup_pe::certificates(&path)
        .expect("the table parses")
        .expect("a table is present");
    assert_eq!(entries.len(), 1);
    // The walk reports the payload length, which excludes the 8-byte header.
    // A length that counted the header too would not fit the declared table size.
    assert_eq!(entries[0].len + 8, range.end - range.start);
}

// # Malformed fixtures
//
// The cases below are a real signed image with one thing changed, because that is
// what a corrupted download or a hostile file actually is: not a hand-built
// nonsense blob but a valid image with a boundary violated.

/// Write `bytes` to a scratch file and return the path.
///
/// The file is named after the calling test, so two tests writing at once do not
/// collide, and it lives in the target directory rather than the system temp
/// directory, which is where a build artefact belongs.
fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("two levels below the root")
        .join("target")
        .join("pe-fixtures");
    std::fs::create_dir_all(&directory).expect("create");
    let path = directory.join(name);
    std::fs::write(&path, bytes).expect("write");
    path
}

/// The digest check is the point of the whole exercise, and a flipped byte inside
/// the section data is the only way to produce it: the certificate table and the
/// PKCS#7 stay intact, so a parser that only looked for a signature structure
/// would still report one.
#[test]
fn an_image_whose_bytes_changed_after_signing_does_not_match() {
    let source = fixture("signed.exe");
    let mut bytes = std::fs::read(&source).expect("read");
    let layout = layout(&bytes);
    // A byte inside the first section, well before the certificate table.
    let section_start = u32::from_le_bytes(
        bytes[layout.section + 20..layout.section + 24]
            .try_into()
            .unwrap(),
    ) as usize;
    assert!(section_start < layout.certificate_table);
    bytes[section_start] ^= 0xff;

    let digest = zup_pe::image_digest(&scratch("flipped.exe", &bytes)).expect("digest");
    let claim = zup_pe::embedded_signature(&source)
        .expect("parses")
        .expect("a signature")
        .digest;
    assert!(
        !claim.matches(&digest),
        "a file whose bytes changed after signing must not match its embedded digest"
    );
}

/// A certificate table whose entry declares a length that runs past the table.
///
/// This must be an *error*, not `None`. "No signature" is the answer a caller
/// would ship a file on, and a file whose certificate table says otherwise must
/// never receive it.
#[test]
fn a_certificate_table_that_overruns_is_refused_rather_than_called_unsigned() {
    let source = fixture("signed.exe");
    let mut bytes = std::fs::read(&source).expect("read");
    let table = layout(&bytes).certificate_table;
    let declared = u32::from_le_bytes(bytes[table..table + 4].try_into().unwrap()) as usize;
    // Declare an entry longer than the table the directory describes.
    bytes[table..table + 4].copy_from_slice(&((declared + 64) as u32).to_le_bytes());

    let path = scratch("overrun.exe", &bytes);
    assert!(
        zup_pe::embedded_signature(&path).is_err(),
        "a truncated image is refused, not reported as unsigned"
    );
    assert!(
        zup_pe::certificates(&path).is_err(),
        "and the walk does not quietly report fewer entries"
    );
}

/// A `WIN_CERTIFICATE` shorter than its own 8-byte header is not a certificate.
#[test]
fn a_certificate_shorter_than_its_header_is_refused() {
    let source = fixture("signed.exe");
    let mut bytes = std::fs::read(&source).expect("read");
    let table = layout(&bytes).certificate_table;
    bytes[table..table + 4].copy_from_slice(&4u32.to_le_bytes());
    let path = scratch("short-cert.exe", &bytes);
    assert!(zup_pe::embedded_signature(&path).is_err());
    assert!(zup_pe::certificates(&path).is_err());
}

/// A table whose declared size is not the sum of its 8-byte-aligned entries is
/// the alignment case: a signer pads the last entry to 8 bytes, so a table size
/// that is not a multiple of the aligned spans is malformed.
#[test]
fn a_certificate_table_whose_size_is_not_a_whole_number_of_aligned_entries_is_refused() {
    let source = fixture("signed.exe");
    let mut bytes = std::fs::read(&source).expect("read");
    let layout = layout(&bytes);
    let table = layout.certificate_table;
    // Shrink the *directory's* size by three bytes, so the table is three bytes
    // shorter than the entries it claims to hold.
    let size_at = layout.data_directory + 32 + 4;
    let size = u32::from_le_bytes(bytes[size_at..size_at + 4].try_into().unwrap());
    bytes[size_at..size_at + 4].copy_from_slice(&(size - 3).to_le_bytes());
    let path = scratch("ragged.exe", &bytes);
    assert!(
        zup_pe::embedded_signature(&path).is_err(),
        "a table that does not fit its own entries is refused"
    );
    assert_eq!(
        table + size as usize,
        bytes.len(),
        "the fixture's table runs to the end of the file"
    );
}

/// A `WIN_CERTIFICATE` at a revision that does not exist, or a type that is not
/// PKCS#7 `SignedData`, is not a signature. Reporting "no signature" is correct
/// here, and the distinction from the malformed cases above is the point: this
/// entry is well-formed and simply not an Authenticode signature.
#[test]
fn a_well_formed_entry_that_is_not_a_signature_is_not_called_one() {
    let source = fixture("signed.exe");
    let mut bytes = std::fs::read(&source).expect("read");
    let table = layout(&bytes).certificate_table;
    // Type 0x0001 is `WIN_CERT_TYPE_X509`, a well-formed entry of a different
    // kind. The length and revision stay valid, so this tests the *kind* check
    // rather than the bounds checks.
    bytes[table + 6..table + 8].copy_from_slice(&1u16.to_le_bytes());
    let path = scratch("x509.exe", &bytes);
    assert_eq!(
        zup_pe::embedded_signature(&path).expect("a well-formed table parses"),
        None,
        "an X509 entry is not an Authenticode signature"
    );
    let entries = zup_pe::certificates(&path)
        .expect("a well-formed table parses")
        .expect("a table is present");
    assert_eq!(entries.len(), 1);
    assert!(
        !entries[0].is_signed_data(),
        "and it does not claim to be one"
    );
}
