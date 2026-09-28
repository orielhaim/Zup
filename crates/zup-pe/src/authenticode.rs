//! Authenticode as a **file format**.
//!
//! A PE image may carry a certificate table, and that table may hold a PKCS#7
//! `SignedData` blob whose `SpcIndirectDataContent` names a digest of the image
//! and an algorithm to compute it with. All of that is structure: it is in the
//! bytes, it is the same on every platform, and a build host on Linux can read
//! all of it.
//!
//! What is *not* here, on purpose: whether Windows trusts the chain, whether the
//! publisher is the one a project expects, whether a timestamp is acceptable,
//! and whether the file is safe to run. Those are answers from a machine's trust
//! store, not properties of the file, and they belong in the Windows adapter -
//! which is also the only place that can ask `WinVerifyTrust`. See
//! `zup-windows::signing`.
//!
//! So the questions this module can answer, and no more:
//!
//! 1. **Is there a certificate table, and what is in it?** [`crate::is_signed`]
//!    and [`certificates`].
//! 2. **What does the signature claim?** [`embedded_signature`] reads the PKCS#7
//!    and reports the digest algorithm and digest value it names. It verifies
//!    nothing.
//! 3. **Do these bytes hash to that claim?** [`image_digest`] computes the image
//!    digest by the rule the specification states. Comparing (2) with (3) is the
//!    structural signature check, and it is the whole of what can be established
//!    without the platform's trust store.
//!
//! # The image digest rule
//!
//! Microsoft's PE specification defines the Authenticode image digest as the hash
//! of the file with three things left out:
//!
//! - the `CheckSum` field in the optional header,
//! - the Certificate Table data directory entry,
//! - the certificate table itself.
//!
//! Everything else is hashed in a specific order: the headers up to
//! `SizeOfHeaders`, then each section's raw data **sorted by
//! `PointerToRawData`**, then the rest of the file up to the certificate table.
//! The sort matters: the file is laid out in link order and loaded in a different
//! one, and a digest computed in file order is not an Authenticode digest.
//!
//! # One implementation, not two
//!
//! The certificate-table walk, the `WIN_CERTIFICATE` validation, the PKCS#7 parse
//! and the image digest are all computed by
//! [`google/authenticode-rs`](https://docs.rs/authenticode), the maintained
//! implementation of exactly this format. This crate contributes the one thing
//! that crate does not have - the image layout, read once and bounds-checked -
//! and a view that hands the certificate table to the walk without loading a
//! gigabyte of image to do it. It does not keep a second parser "for safety": a
//! format implemented twice is a format with two answers, and only the tests
//! between them notice.

use std::ops::Range;
use std::path::Path;

use authenticode::{AttributeCertificateIterator, AuthenticodeSignature};
pub use authenticode::{PeOffsetError, PeOffsets, PeTrait, authenticode_digest};
use cms::signed_data::SignerIdentifier;
use der::Encode;
use sha2::Digest;
use x509_cert::name::RelativeDistinguishedName;

use crate::{Image, PeError};

/// The current `WIN_CERTIFICATE` revision.
const WIN_CERT_REVISION_2_0: u16 = 0x0200;
/// `WIN_CERT_TYPE_PKCS_SIGNED_DATA`: the certificate contains a PKCS#7
/// `SignedData`, which is what Authenticode puts there.
const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;

/// The digest algorithm a signature names.
///
/// Reported as the algorithm the signature itself carries, because the point of
/// reading it is to notice a signature that named something other than what zup
/// measured with. A closed enum means "something else" cannot be rendered as a
/// name zup has an opinion about: it is [`DigestAlgorithm::Other`], and the
/// caller decides whether refusing is right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    /// An algorithm zup does not name.
    Other,
}

impl DigestAlgorithm {
    /// The name, as a signature would spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Other => "unknown",
        }
    }

    /// Read an algorithm identifier's OID.
    ///
    /// The Authenticode digest algorithm is an `AlgorithmIdentifier`, so its name
    /// is the OID inside. These are the digest OIDs from RFC 5915, which is where
    /// the ones a signer writes are defined.
    fn from_oid(oid: &[u8]) -> Self {
        match oid {
            // id-sha1
            [0x2b, 0x0e, 0x03, 0x02, 0x1a] => Self::Sha1,
            // id-sha256
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01] => Self::Sha256,
            // id-sha384
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02] => Self::Sha384,
            // id-sha512
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03] => Self::Sha512,
            _ => Self::Other,
        }
    }
}

/// The digest an image's signature claims over that image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureDigest {
    /// The algorithm the signature says to compute the digest with.
    pub algorithm: DigestAlgorithm,
    /// The claim itself. It is not correct until it is compared with
    /// [`image_digest`].
    pub value: Vec<u8>,
}

impl SignatureDigest {
    /// The claim as lowercase hex, for a report.
    pub fn to_hex(&self) -> String {
        self.value
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Whether this claim is the SHA-256 image digest of `digest`.
    ///
    /// The length is compared as well, so a truncated or padded claim is a
    /// mismatch rather than a prefix of one.
    pub fn matches(&self, digest: &zup_core::Sha256Digest) -> bool {
        self.algorithm == DigestAlgorithm::Sha256
            && self.value.len() == 32
            && self.value == digest.as_bytes()
    }
}

/// One `WIN_CERTIFICATE` entry in an image's certificate table.
///
/// The header is 8 bytes and the entries are 8-byte aligned, so the gap between
/// two entries is padding and belongs to neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Certificate {
    /// `WIN_CERTIFICATE` revision. `0x0200` is the only one that exists.
    pub revision: u16,
    /// `WIN_CERTIFICATE` type. `0x0002` is a PKCS#7 `SignedData`, which is what
    /// Authenticode uses.
    pub certificate_type: u16,
    /// The entry's payload length, excluding the 8-byte header.
    pub len: usize,
}

impl Certificate {
    /// Whether this entry is a PKCS#7 `SignedData` at the current revision,
    /// which is the one Authenticode puts there.
    pub fn is_signed_data(&self) -> bool {
        self.revision == WIN_CERT_REVISION_2_0
            && self.certificate_type == WIN_CERT_TYPE_PKCS_SIGNED_DATA
    }
}

impl From<&authenticode::AttributeCertificate<'_>> for Certificate {
    fn from(entry: &authenticode::AttributeCertificate<'_>) -> Self {
        Self {
            revision: entry.revision,
            certificate_type: entry.certificate_type,
            len: entry.data.len(),
        }
    }
}

/// What an image's certificate table holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSignature {
    /// The entries the table holds, in file order.
    pub certificates: Vec<Certificate>,
    /// The digest the signature claims over the image.
    pub digest: SignatureDigest,
}

/// The bytes of the image's Authenticode PKCS#7 blob.
///
/// The blob is the file's own content, and three different questions are asked of
/// it: is it there, does the digest it carries cover these bytes, and does it
/// carry a countersignature. All three are structural, and all three are
/// answered from the certificate table rather than from a platform API - which is
/// what lets a Linux build host answer them.
pub fn signature_blob(path: &Path) -> Option<Vec<u8>> {
    let table = table_bytes(path).ok()??;
    read_table(&table).ok()?.blob
}

/// The signing certificate an image's Authenticode signature names, and the
/// identity a release description would record from it.
///
/// The `SignerInfo` inside the blob says *which* certificate signed, by issuer and
/// serial number, and the blob carries the certificates. A file's certificate
/// table therefore answers "who signed this" on its own, on any platform, with no
/// certificate store involved - which matters because a store for a timestamped
/// file also holds the timestamping authority's certificate, and "the first
/// certificate Windows hands back" is the TSA's.
///
/// The subject is rendered from the certificate's own `Name`, so it is the same
/// string on every host and in every document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningCertificate {
    /// The certificate's subject, rendered from its own `Name`.
    pub subject: String,
    /// The certificate's issuer, rendered the same way.
    pub issuer: String,
    /// The certificate's serial number, big-endian, without a DER sign byte.
    pub serial_number: Vec<u8>,
    /// A digest of the certificate's own DER encoding, uppercase hex.
    ///
    /// The certificate's identity, not the file's. Windows calls this a
    /// thumbprint and computes it with SHA-1, which is a fingerprint of a
    /// certificate and not a digest of a file; zup records SHA-256 because every
    /// other digest in a release description is SHA-256 and a reader who sees two
    /// algorithms spelled the same way should not have to wonder which is which.
    pub fingerprint: String,
}

/// The signing certificate an image's Authenticode signature names.
pub fn signing_certificate(path: &Path) -> Option<SigningCertificate> {
    use sha2::Digest;
    use x509_cert::serial_number::SerialNumber as X509Serial;

    let table = table_bytes(path).ok()??;
    let blob = read_table(&table).ok()?.blob?;
    let signature = AuthenticodeSignature::from_bytes(&blob).ok()?;
    let signer = match &signature.signer_info().sid {
        SignerIdentifier::IssuerAndSerialNumber(identifier) => identifier,
        // A `SubjectKeyIdentifier` names the certificate by its public key
        // instead, which is a valid CMS shape and is not what a signing tool
        // emits. Reporting nothing is the honest answer: the caller wants the
        // publisher, and this does not identify one.
        SignerIdentifier::SubjectKeyIdentifier(_) => return None,
    };
    // The `SignerInfo` names the signer by its serial number, and the blob carries
    // the certificates, so the signer is *found* rather than assumed. A
    // timestamped signature also carries the timestamping authority's
    // certificate, and an implementation that took the first certificate it found
    // would name the TSA as the publisher.
    let wanted = X509Serial::new(&serial_bytes(&signer.serial_number.to_der().ok()?)).ok()?;
    let certificate = signature
        .certificates()
        .find(|certificate| certificate.tbs_certificate.serial_number == wanted)?;
    let der = certificate.to_der().ok()?;
    Some(SigningCertificate {
        subject: render(certificate.tbs_certificate.subject.as_ref()),
        issuer: render(certificate.tbs_certificate.issuer.as_ref()),
        serial_number: wanted.as_bytes().to_vec(),
        fingerprint: sha2::Sha256::digest(&der)
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect(),
    })
}

/// A certificate's DER `INTEGER` serial, as the value's bytes.
///
/// DER integers are signed, so a serial whose top bit is set is encoded with a
/// leading `0x00` that is a sign marker rather than part of the value. Two
/// spellings of one serial number have to compare equal, so the marker goes.
fn serial_bytes(encoded: &[u8]) -> Vec<u8> {
    if encoded.len() < 2 || encoded[0] != 0x02 {
        return Vec::new();
    }
    let mut body = &encoded[2..];
    while body.first() == Some(&0x00) && body.len() > 1 {
        body = &body[1..];
    }
    body.to_vec()
}

/// An X.509 `Name` as `oid=value` pairs joined by `,`.
///
/// The pairs come out in the certificate's own order and the values are decoded
/// from the ASN.1 `ANY` they are stored as, so a name is stable and diffable
/// rather than dependent on a platform's display code page. A value whose type
/// is not one zup can name is rendered as its DER hex, which is worse than
/// pretty and better than ambiguous.
fn render(name: &[RelativeDistinguishedName]) -> String {
    name.iter()
        .map(|rdn| {
            rdn.0
                .iter()
                .map(|attribute| {
                    let value = attribute
                        .value
                        .decode_as::<der::asn1::Utf8StringRef>()
                        .map(|text| text.as_str().to_owned())
                        .or_else(|_| {
                            attribute
                                .value
                                .decode_as::<der::asn1::PrintableStringRef>()
                                .map(|text| text.as_str().to_owned())
                        })
                        .unwrap_or_else(|_| {
                            attribute.value.to_der().map_or_else(
                                |_| String::from("<unreadable>"),
                                |der| der.iter().map(|byte| format!("{byte:02x}")).collect(),
                            )
                        });
                    format!("{}={value}", attribute.oid)
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The image's certificate table, read as a walk over `WIN_CERTIFICATE` entries.
///
/// Reads only the table, which is a few kilobytes, out of a file that may be a
/// gigabyte: the header says where the table is and the rest of the image is
/// irrelevant to it. An image with no table is `Ok(None)` - a fact about the
/// image, not a failure.
pub fn certificates(path: &Path) -> Result<Option<Vec<Certificate>>, PeError> {
    match table_bytes(path)? {
        Some(table) => Ok(Some(read_table(&table)?.certificates)),
        None => Ok(None),
    }
}

/// Read the Authenticode signature an image carries, if it has one.
///
/// Reads and parses the PKCS#7 without verifying anything. The digest it returns
/// is *what the signature claims*; the only way to know whether the claim is true
/// is to compare it with [`image_digest`].
///
/// A table that declares a signed-data entry and does not contain a parseable
/// signature is an error, not a `None`. A `None` means "there is no signature
/// here", and a caller must not be able to hear that about a file whose bytes
/// claim otherwise.
pub fn embedded_signature(path: &Path) -> Result<Option<EmbeddedSignature>, PeError> {
    let Some(table) = table_bytes(path)? else {
        return Ok(None);
    };
    Ok(read_table(&table)?.signature)
}

/// The Authenticode image digest of `path`, by the rule the specification states.
///
/// Loads the whole image. The digest reads the header, then the sections in
/// ascending file order, then the rest of the file; those are disjoint regions
/// and the only way to hand them to the routine that hashes them is to have the
/// file. A caller that only needs to know whether a certificate table exists
/// should use [`crate::is_signed`], which seeks.
pub fn image_digest(path: &Path) -> Result<zup_core::Sha256Digest, PeError> {
    Image::read(path)?.authenticode_digest()
}

impl Image {
    /// The Authenticode image digest of these bytes.
    pub fn authenticode_digest(&self) -> Result<zup_core::Sha256Digest, PeError> {
        let mut hasher = Sha256Bridge(sha2::Sha256::new());
        authenticode_digest(self, &mut hasher).map_err(|_| PeError::Invalid)?;
        Ok(zup_core::Sha256Digest::from_bytes(
            hasher.0.finalize().into(),
        ))
    }
}

/// Read an image's certificate table, or `None` when the directory is zeroed.
fn table_bytes(path: &Path) -> Result<Option<Vec<u8>>, PeError> {
    use std::io::{Read, Seek, SeekFrom};

    let Some(range) = crate::read_pe_header(path)?.certificate_table() else {
        return Ok(None);
    };
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(range.start as u64))?;
    let mut table = vec![0u8; range.end - range.start];
    file.read_exact(&mut table)?;
    Ok(Some(table))
}

/// Everything an image's certificate table says.
struct Table {
    /// The `WIN_CERTIFICATE` entries, in file order.
    certificates: Vec<Certificate>,
    /// The signature the first signed-data entry carries, if there is one.
    signature: Option<EmbeddedSignature>,
    /// The signed-data entry's raw PKCS#7 bytes, if there is one.
    blob: Option<Vec<u8>>,
}

/// Walk a certificate table, and read the signature out of it.
///
/// A table that declares a signed-data entry whose payload is not a parseable
/// PKCS#7 is an error. Reporting it as "no signature" is the one answer a caller
/// of this function must not receive: a caller that hears "unsigned" about a file
/// whose certificate table says otherwise has been told to ship it.
fn read_table(table: &[u8]) -> Result<Table, PeError> {
    let view = CertificateTableView { table };
    let Some(iter) = AttributeCertificateIterator::new(&view).map_err(|_| PeError::Invalid)? else {
        return Ok(Table {
            certificates: Vec::new(),
            signature: None,
            blob: None,
        });
    };
    let mut certificates = Vec::new();
    let mut digest = None;
    let mut blob = None;
    for entry in iter {
        let entry = entry.map_err(|_| PeError::Invalid)?;
        let certificate = Certificate::from(&entry);
        let signed_data = certificate.is_signed_data();
        certificates.push(certificate);
        if signed_data && digest.is_none() {
            let parsed = entry
                .get_authenticode_signature()
                .map_err(|_| PeError::Invalid)?;
            digest = Some(SignatureDigest {
                algorithm: DigestAlgorithm::from_oid(parsed.digest_algorithm().oid.as_bytes()),
                value: parsed.digest().to_vec(),
            });
            blob = Some(entry.data.to_vec());
        }
    }
    let signature = digest.map(|digest| EmbeddedSignature {
        certificates: certificates.clone(),
        digest,
    });
    Ok(Table {
        certificates,
        signature,
        blob,
    })
}

/// The certificate table, presented to the `authenticode` crate as if it were a
/// whole image.
///
/// The crate's `AttributeCertificateIterator` reads exactly two things - the
/// image bytes and the certificate table's range - and the table is the only part
/// of the image it needs. Presenting the table as an image whose certificate
/// table is its entire length is therefore a way of handing the walk a few
/// kilobytes instead of a gigabyte, not a different answer. The section and
/// offset methods are never reached by a certificate walk, and refuse rather than
/// invent a layout this view does not have.
struct CertificateTableView<'a> {
    table: &'a [u8],
}

impl PeTrait for CertificateTableView<'_> {
    fn data(&self) -> &[u8] {
        self.table
    }

    fn num_sections(&self) -> usize {
        0
    }

    fn section_data_range(&self, _index: usize) -> Result<Range<usize>, PeOffsetError> {
        Err(PeOffsetError)
    }

    fn certificate_table_range(&self) -> Result<Option<Range<usize>>, PeOffsetError> {
        Ok(Some(0..self.table.len()))
    }

    fn offsets(&self) -> Result<PeOffsets, PeOffsetError> {
        Err(PeOffsetError)
    }
}

/// The `digest` 0.10 interface the `authenticode` crate was published against,
/// fed by the SHA-256 zup uses everywhere.
///
/// `authenticode` 0.6 depends on `digest` 0.10 and zup is on `sha2` 0.11, which
/// implements `digest` 0.11. Two incompatible releases of one trait, so the
/// bridge is a local newtype - and not an `impl Update for Sha256`, which the
/// orphan rule would refuse in any case. The duplicate `digest` this leaves in
/// the graph is upstream's version skew rather than a second hash implementation,
/// and it costs one crate.
struct Sha256Bridge(sha2::Sha256);

impl digest::Update for Sha256Bridge {
    fn update(&mut self, data: &[u8]) {
        Digest::update(&mut self.0, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_algorithms_are_read_from_the_oid_the_signature_carries() {
        assert_eq!(
            DigestAlgorithm::from_oid(&[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01]),
            DigestAlgorithm::Sha256
        );
        assert_eq!(
            DigestAlgorithm::from_oid(&[0x2b, 0x0e, 0x03, 0x02, 0x1a]),
            DigestAlgorithm::Sha1
        );
        assert_eq!(
            DigestAlgorithm::from_oid(&[0x01, 0x02]),
            DigestAlgorithm::Other,
            "an algorithm zup does not name is not guessed at"
        );
    }

    /// A claim is a claim. It is not correct until it is compared with the
    /// image, and a claim of the wrong length is a mismatch rather than a prefix
    /// of one.
    #[test]
    fn a_digest_claim_matches_only_its_own_algorithm_and_length() {
        let real = zup_core::hash_bytes(b"image");
        let claim = SignatureDigest {
            algorithm: DigestAlgorithm::Sha256,
            value: real.as_bytes().to_vec(),
        };
        assert!(claim.matches(&real));
        for wrong in [
            SignatureDigest {
                algorithm: DigestAlgorithm::Sha1,
                value: real.as_bytes().to_vec(),
            },
            SignatureDigest {
                algorithm: DigestAlgorithm::Sha256,
                value: real.as_bytes()[..31].to_vec(),
            },
        ] {
            assert!(!wrong.matches(&real));
        }
    }
}
