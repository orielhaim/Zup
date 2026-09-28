//! Windows signature verification: the only place that can ask Windows.
//!
//! # Why this is not in `zup-pe`
//!
//! A PE image may carry a signature, and reading one is a file-format question -
//! `zup-pe` answers it, on any platform, with a maintained parser. Whether
//! *Windows* accepts that signature is a different question with a different
//! authority: it is a judgement by the verifying machine about its own
//! certificate store, its own policy, and its own revocation state, and only
//! `WinVerifyTrust` can produce it. Reproducing it would mean reimplementing
//! Windows trust, which is both impossible to get right and wrong to try: a
//! reimplementation that agreed with Windows today would be a second thing to
//! keep in agreement tomorrow.
//!
//! So the split is by authority, not by convenience:
//!
//! - `zup-pe::authenticode` - is there a signature structure, and does its
//!   digest match these bytes? A fact about the file.
//! - `zup-windows::signing` - does Windows trust the chain, whose is it, and is
//!   it timestamped? A fact about the machine.
//!
//! And the release policy is stated in terms of the second, because that is the
//! question a person downloading an installer actually has.
//!
//! # The three answers
//!
//! Verification answers three separate questions rather than one boolean,
//! because they fail separately and matter differently:
//!
//! 1. **Is there a signature at all?** Read from the certificate table.
//! 2. **Does the signature cover these bytes?** `WinVerifyTrust` recomputes the
//!    message digest. A file whose bytes changed after signing fails here, and
//!    that is the one failure that makes an artifact dangerous.
//! 3. **Whose is it, and was it timestamped?** The signer's certificate, and
//!    whether an RFC 3161 countersignature is present.
//!
//! A self-signed development certificate fails (3)'s chain while passing (2)
//! perfectly, so requiring full trust in a unit test would mean requiring a
//! production certificate to run `cargo test`.
//!
//! # What the timestamp check does and does not prove
//!
//! Detecting an RFC 3161 countersignature scans the embedded PKCS#7 for the DER
//! encoding of `szOID_RFC3161_counterSign`. Those attributes live in the
//! *unauthenticated* attributes of the `SignerInfo`, so they appear in cleartext
//! inside the signature blob. The scan therefore proves **presence** and nothing
//! more: it does not check the timestamp token, its digest algorithm, or its
//! ordering against the signing certificate. Integrity of the countersignature is
//! `WinVerifyTrust`'s answer; this check exists to enforce a *policy* - "no
//! production artifact ships without an RFC 3161 timestamp" - for which presence
//! is the right granularity. The legacy Authenticode timestamp is detected
//! separately so a policy can refuse it by name rather than silently accept it.

use std::path::{Path, PathBuf};

use zup_signing::{EvidenceFact, SigningEvidence};

/// A signer certificate's identity, as far as a build needs to tell two apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerIdentity {
    /// The certificate subject, in Windows display form.
    pub subject: String,
    /// The certificate issuer, in Windows display form.
    pub issuer: String,
    /// The certificate's SHA-1 thumbprint, uppercase hex, as Windows prints it.
    ///
    /// SHA-1 here identifies a *certificate*, which is what a thumbprint has
    /// always meant. It says nothing about the file's digest, which is SHA-256
    /// everywhere in zup.
    pub thumbprint: String,
}

impl SignerIdentity {
    /// Whether this identity's subject contains `fragment`.
    ///
    /// A normalized substring rather than an exact match, because a production
    /// subject carries an O, a C, and a jurisdiction that differ per entity, and
    /// a policy that had to spell the whole distinguished name would break on
    /// every renewal. Normalizing means `"Acme Inc."` and `"Acme, Inc."` do not
    /// read as two publishers.
    pub fn subject_contains(&self, fragment: &str) -> bool {
        if fragment.is_empty() {
            return false;
        }
        normalize_subject(&self.subject).contains(&normalize_subject(fragment))
    }

    /// Whether this identity's thumbprint is `expected`, ignoring case and
    /// separators.
    pub fn thumbprint_matches(&self, expected: &str) -> bool {
        let wanted: String = expected
            .chars()
            .filter(|character| character.is_ascii_hexdigit())
            .map(|character| character.to_ascii_uppercase())
            .collect();
        !wanted.is_empty() && self.thumbprint == wanted
    }
}

/// Drop the punctuation and spacing a subject carries for humans, so two
/// spellings of one publisher compare equal.
fn normalize_subject(subject: &str) -> String {
    subject
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// The timestamp evidence in one signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timestamp {
    /// An RFC 3161 countersignature is present: `signtool /tr <tsa> /td SHA256`.
    ///
    /// What a production signature is expected to carry. The signing certificate
    /// can expire while the artifact is still installed, and the countersignature
    /// is what keeps it valid after that.
    Rfc3161,
    /// Only the legacy Authenticode timestamp is present: `signtool /t <server>`.
    ///
    /// Detected so a policy can refuse it by name. It is not a weaker claim than
    /// it sounds - the countersignature is still inside the signed bytes - but it
    /// is not what current Windows guidance asks for.
    LegacyOnly,
    /// No countersignature at all. The signature stops validating when the
    /// certificate expires.
    None,
}

impl Timestamp {
    /// The stable name recorded as signing evidence.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rfc3161 => "rfc3161",
            Self::LegacyOnly => "legacy",
            Self::None => "none",
        }
    }
}

/// Why Windows refused a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustFailure {
    /// The message digest does not match the signature. The file changed after
    /// it was signed, or the signature was built over different bytes.
    ///
    /// The only failure that is unconditionally fatal. Every other value here is
    /// a statement about trust; this one is a statement about the artifact.
    BadDigest,
    /// The signing certificate is outside its validity window.
    Expired,
    /// The signing certificate was revoked, or a revocation lookup could not
    /// complete.
    Revoked,
    /// The signature is not one Windows can evaluate: an unsupported digest or
    /// encoding, a file that is not a signed image, or a counterSignature whose
    /// timestamp is not ordered correctly against the certificate.
    Unsupported,
    /// Anything else Windows reported.
    Other(u32),
}

/// What verification found in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authenticode {
    /// The file carries no signature at all. It was never signed.
    Unsigned,
    /// The file is signed and the signature covers these bytes.
    ///
    /// Whether the chain is *trusted* is a separate fact on the image, because a
    /// self-signed development certificate produces a sound signature from an
    /// untrusted chain, and conflating the two would force every test to own a
    /// production certificate.
    Signed(SignedImage),
    /// The file is signed, and something about it is wrong: the bytes do not
    /// verify, the certificate is expired or revoked, or the signature cannot be
    /// evaluated. Never ship this.
    Damaged {
        /// The signer, when the certificate could still be read.
        identity: Option<SignerIdentity>,
        /// Why Windows refused it.
        failure: TrustFailure,
    },
}

/// A signature that verified against the file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedImage {
    /// Who signed it.
    pub identity: SignerIdentity,
    /// Windows built a chain from the signing certificate to a root it trusts.
    pub trusted_chain: bool,
    /// The timestamp evidence in the signature.
    pub timestamp: Timestamp,
}

/// What a signature must satisfy.
///
/// The defaults are the production policy, matching current Microsoft guidance,
/// and they are the *only* defaults. A test that needs something weaker says so
/// in the test, so the product never ships a laxer check for everyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePolicy {
    /// Require Windows to build a chain to a root it trusts.
    pub require_trusted_chain: bool,
    /// Require an RFC 3161 countersignature.
    pub require_rfc3161_timestamp: bool,
    /// Refuse the legacy Authenticode timestamp even when one is present.
    pub reject_legacy_timestamp: bool,
    /// A subject the signer must carry, matched as a normalized substring.
    pub subject: Option<String>,
    /// A thumbprint the signer must carry, compared exactly.
    pub thumbprint: Option<String>,
    /// Whether revocation may be checked over the network.
    ///
    /// Off by default. A build host behind a firewall must not fail an otherwise
    /// valid signature because a CRL was unreachable, and cache-only retrieval
    /// is what makes verification offline-safe. A release pipeline that has the
    /// network can turn it on deliberately.
    pub online_revocation: bool,
}

impl Default for SignaturePolicy {
    fn default() -> Self {
        Self {
            require_trusted_chain: true,
            require_rfc3161_timestamp: true,
            reject_legacy_timestamp: true,
            subject: None,
            thumbprint: None,
            online_revocation: false,
        }
    }
}

impl SignaturePolicy {
    /// A policy for a local test certificate: a self-signed chain, no TSA.
    pub fn test_identity() -> Self {
        Self {
            require_trusted_chain: false,
            require_rfc3161_timestamp: false,
            reject_legacy_timestamp: false,
            subject: None,
            thumbprint: None,
            online_revocation: false,
        }
    }

    /// Require this publisher, by subject.
    pub fn signed_by(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Require this exact certificate.
    pub fn thumbprint(mut self, thumbprint: impl Into<String>) -> Self {
        self.thumbprint = Some(thumbprint.into());
        self
    }

    /// Whether the signature in `image` satisfies this policy.
    pub fn accepts(&self, image: &SignedImage) -> Result<(), VerificationError> {
        if self.require_trusted_chain && !image.trusted_chain {
            return Err(VerificationError::UntrustedChain);
        }
        match image.timestamp {
            Timestamp::Rfc3161 => {}
            Timestamp::LegacyOnly if self.reject_legacy_timestamp => {
                return Err(VerificationError::LegacyTimestamp);
            }
            Timestamp::None if self.require_rfc3161_timestamp => {
                return Err(VerificationError::MissingTimestamp);
            }
            Timestamp::LegacyOnly | Timestamp::None => {}
        }
        if let Some(wanted) = &self.subject
            && !image.identity.subject_contains(wanted)
        {
            return Err(VerificationError::WrongSubject {
                found: image.identity.subject.clone(),
            });
        }
        if let Some(wanted) = &self.thumbprint
            && !image.identity.thumbprint_matches(wanted)
        {
            return Err(VerificationError::WrongThumbprint {
                found: image.identity.thumbprint.clone(),
            });
        }
        Ok(())
    }
}

/// A file whose signature was read and accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    /// The file that was verified.
    pub path: PathBuf,
    /// What its signature says.
    pub image: SignedImage,
}

impl VerifiedFile {
    /// The signing evidence to record against this file.
    ///
    /// The portable vocabulary, populated from the platform's own answer. Note
    /// what is *not* here: no certificate bytes, no signature blob, no
    /// attestation. A release description records what was established, and
    /// everything on it is a public identity or a fact about a public identity.
    pub fn evidence(&self) -> Vec<SigningEvidence> {
        let mut evidence = vec![SigningEvidence::new(
            EvidenceFact::SignatureCoversBytes,
            "sha256",
        )];
        if self.image.trusted_chain {
            evidence.push(SigningEvidence::new(
                EvidenceFact::PlatformTrustAccepted,
                "windows",
            ));
        }
        evidence.push(SigningEvidence::new(
            EvidenceFact::Publisher,
            self.image.identity.subject.clone(),
        ));
        evidence.push(SigningEvidence::new(
            EvidenceFact::Certificate,
            self.image.identity.thumbprint.clone(),
        ));
        evidence.push(SigningEvidence::new(
            EvidenceFact::Timestamp,
            self.image.timestamp.as_str(),
        ));
        evidence
    }
}

/// Read `path`'s signature and report what it is, without deciding whether that
/// is acceptable.
///
/// [`verify`] is the gate. This is what a report, a doctor row, and a test that
/// wants to observe a signature rather than enforce one use.
pub fn inspect(path: &Path) -> Result<Authenticode, VerificationError> {
    if !path.is_file() {
        return Err(VerificationError::Unreadable {
            path: path.to_path_buf(),
        });
    }
    platform::inspect(path)
}

/// Verify `path` under `policy`.
///
/// One function, because the two halves are not separable in practice: a file
/// that is unsigned and a file whose signature is wrong are both a refusal, and a
/// caller should not be able to forget to apply the second.
pub fn verify(path: &Path, policy: &SignaturePolicy) -> Result<VerifiedFile, VerificationError> {
    match inspect(path)? {
        Authenticode::Unsigned => Err(VerificationError::Unsigned(path.to_path_buf())),
        Authenticode::Damaged { identity, failure } => {
            Err(VerificationError::Refused { failure, identity })
        }
        Authenticode::Signed(image) => {
            policy.accepts(&image)?;
            Ok(VerifiedFile {
                path: path.to_path_buf(),
                image,
            })
        }
    }
}

/// Why a signature was refused.
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("`{path}` is not a file")]
    Unreadable { path: PathBuf },
    #[error("`{0}` carries no Authenticode signature")]
    Unsigned(PathBuf),
    #[error("`{path}` could not be read: {reason}")]
    Read { path: PathBuf, reason: String },
    #[error("the signature does not cover these bytes: {failure:?}")]
    Refused {
        failure: TrustFailure,
        identity: Option<SignerIdentity>,
    },
    #[error("the signing certificate does not chain to a trusted root")]
    UntrustedChain,
    #[error("the signature has no RFC 3161 timestamp")]
    MissingTimestamp,
    #[error("the signature carries only the legacy Authenticode timestamp")]
    LegacyTimestamp,
    #[error("the signature is from `{found}`, which is not the expected publisher")]
    WrongSubject { found: String },
    #[error("the signing certificate is `{found}`, which is not the expected certificate")]
    WrongThumbprint { found: String },
    #[error("Authenticode verification is only available on Windows")]
    Unsupported,
}

#[cfg(windows)]
mod platform {
    //! The one call that answers the question this module exists for.
    //!
    //! Everything else - the certificate table, the signature structure, the
    //! digest, the publisher, the timestamp - is read by `zup-pe` from the file
    //! itself, and would give the same answer on any host. What is left is
    //! `WinVerifyTrust`, which asks the verifying machine's trust store, and which
    //! no amount of parsing can stand in for.
    //!
    //! So the module is one call and the classification of its answer, and the
    //! reason it is not shorter is that `WinVerifyTrust` is a two-call state
    //! machine whose second call releases what the first allocated.

    use std::ffi::c_void;
    use std::path::Path;

    use windows_link::link;

    use super::{
        Authenticode, SignedImage, SignerIdentity, Timestamp, TrustFailure, VerificationError,
    };

    type Long = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    type Guid = [u8; 16];

    link!("wintrust.dll" "system" fn WinVerifyTrust(window: *mut c_void, action: *const Guid, data: *mut c_void) -> Long);

    /// `WINTRUST_ACTION_GENERIC_VERIFY_V2` = `{00AAC56B-CD44-11d0-8CC2-00C04FC295EE}`,
    /// in the byte order `WinVerifyTrust` receives it: the first three fields
    /// little-endian, then the last eight bytes as written.
    ///
    /// The first `Data1` word is `0xaac56b`, so the first two bytes on the wire
    /// are `0x6b, 0xc5` - **`0xc5`, not `0x56`**. Getting that byte wrong does not
    /// come back as "untrusted": `WinVerifyTrust` cannot find a policy provider
    /// for the action id and answers `TRUST_E_PROVIDER_UNKNOWN` for *every* file,
    /// signed or not, which is a very quiet way to ship a verification gate that
    /// has never once verified anything.
    const ACTION_GENERIC_VERIFY_V2: Guid = [
        0x6b, 0xc5, 0xaa, 0x00, 0x44, 0xcd, 0xd0, 0x11, 0x8c, 0xc2, 0x00, 0xc0, 0x4f, 0xc2, 0x95,
        0xee,
    ];

    const WTD_UI_NONE: Dword = 2;
    const WTD_REVOKE_NONE: Dword = 0;
    const WTD_CHOICE_FILE: Dword = 1;
    const WTD_STATEACTION_VERIFY: Dword = 1;
    const WTD_STATEACTION_CLOSE: Dword = 2;
    /// Retrieve revocation data from the local cache only, so a host without
    /// network access reaches the same answer as one with it.
    const WTD_CACHE_ONLY_URL_RETRIEVAL: Dword = 0x0000_1000;

    const TRUST_E_NOSIGNATURE: i32 = 0x800B_0100_u32 as i32;
    const CERT_E_EXPIRED: i32 = 0x800B_0101_u32 as i32;
    const TRUST_E_SUBJECT_NOT_TRUSTED: i32 = 0x800B_0004_u32 as i32;
    const TRUST_E_SUBJECT_FORM_UNKNOWN: i32 = 0x800B_0103_u32 as i32;
    const CERT_E_UNTRUSTEDROOT: i32 = 0x800B_0109_u32 as i32;
    const CERT_E_REVOKED: i32 = 0x800B_010C_u32 as i32;
    const TRUST_E_UNTRUSTEDTESTROOT: i32 = 0x800B_010D_u32 as i32;
    const CERT_E_REVOCATION_FAILURE: i32 = 0x800B_010E_u32 as i32;
    const TRUST_E_BAD_DIGEST: i32 = 0x8009_6010_u32 as i32;
    const CRYPT_E_FILE_ERROR: i32 = 0x8009_6006_u32 as i32;

    /// `szOID_RFC3161_counterSign` = `1.3.6.1.4.1.311.3.3.1`, DER.
    const OID_RFC3161: &[u8] = &[
        0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x96, 0x37, 0x03, 0x03, 0x01,
    ];
    /// `szOID_PKIX_COUNTERSIGN` = `1.2.840.113549.1.9.6`, DER.
    const OID_PKIX_COUNTERSIGN: &[u8] = &[
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x06,
    ];

    /// `WINTRUST_FILE_INFO`.
    #[repr(C)]
    struct WinTrustFileInfo {
        cb_struct: Dword,
        pcwsz_file_path: *const u16,
        h_file: Handle,
        pg_known_subject: *const Guid,
    }

    /// `WINTRUST_DATA`.
    ///
    /// `cb_struct` is computed from this type, so a field a later SDK added is
    /// simply not claimed: `WinVerifyTrust` reads only what `cb_struct` says is
    /// present, and the layout up to `dw_ui_context` has been stable since
    /// Windows 7. The explicit padding is what `#[repr(C)]` inserts anyway; it is
    /// written out because a reader comparing this to the SDK header should not
    /// have to derive it.
    #[repr(C)]
    struct WinTrustData {
        cb_struct: Dword,
        _pad0: Dword,
        p_policy_callback_data: *mut c_void,
        p_sip_client_data: *mut c_void,
        dw_ui_choice: Dword,
        fdw_revocation_checks: Dword,
        dw_union_choice: Dword,
        _pad1: Dword,
        p_file: *const WinTrustFileInfo,
        dw_state_action: Dword,
        _pad2: Dword,
        h_wvt_state_data: Handle,
        pwsz_url_reference: *const u16,
        dw_prov_flags: Dword,
        dw_ui_context: Dword,
    }

    /// Ask Windows to verify one file's Authenticode signature.
    ///
    /// The wide path and the `WINTRUST_FILE_INFO` are locals precisely so they
    /// outlive the calls that point at them: the borrow checker is what guarantees
    /// the buffer is alive for the whole verification, where a struct field read
    /// through a raw pointer would have to be asserted instead.
    ///
    /// Verification is a state machine. `VERIFY` recomputes the message digest and
    /// builds the chain; `CLOSE` releases the state the first call allocated, and
    /// skipping it leaks that state for the process's lifetime. So the close is a
    /// plain statement between the two, not a `Drop`.
    fn trust(path: &Path) -> i32 {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let file = WinTrustFileInfo {
            cb_struct: std::mem::size_of::<WinTrustFileInfo>() as Dword,
            pcwsz_file_path: wide.as_ptr(),
            h_file: std::ptr::null_mut(),
            pg_known_subject: std::ptr::null(),
        };
        let mut data = WinTrustData {
            cb_struct: std::mem::size_of::<WinTrustData>() as Dword,
            _pad0: 0,
            p_policy_callback_data: std::ptr::null_mut(),
            p_sip_client_data: std::ptr::null_mut(),
            dw_ui_choice: WTD_UI_NONE,
            fdw_revocation_checks: WTD_REVOKE_NONE,
            dw_union_choice: WTD_CHOICE_FILE,
            _pad1: 0,
            p_file: &file,
            dw_state_action: WTD_STATEACTION_VERIFY,
            _pad2: 0,
            h_wvt_state_data: std::ptr::null_mut(),
            pwsz_url_reference: std::ptr::null(),
            dw_prov_flags: WTD_CACHE_ONLY_URL_RETRIEVAL,
            dw_ui_context: 0,
        };
        // SAFETY: `data` is a fully initialized `WINTRUST_DATA` whose `p_file`
        // borrows a live local and whose `pcwsz_file_path` points into a live,
        // NUL-terminated buffer. The action GUID is a `'static` constant of the
        // documented length. The two calls form one verify/close pair.
        unsafe {
            let result = WinVerifyTrust(
                std::ptr::null_mut(),
                &ACTION_GENERIC_VERIFY_V2,
                (&mut data as *mut WinTrustData).cast(),
            );
            data.dw_state_action = WTD_STATEACTION_CLOSE;
            WinVerifyTrust(
                std::ptr::null_mut(),
                &ACTION_GENERIC_VERIFY_V2,
                (&mut data as *mut WinTrustData).cast(),
            );
            result
        }
    }

    /// How a `WinVerifyTrust` answer maps onto what a build must know.
    enum Verdict {
        /// The chain is sound and trusted.
        Trusted,
        /// The signature covers these bytes, and the chain is not trusted.
        ///
        /// What a self-signed development certificate produces, and the reason
        /// [`super::SignaturePolicy::test_identity`] exists.
        UntrustedChain,
        /// No signature.
        Unsigned,
        /// The bytes, the certificate, or the signature itself is wrong.
        Damaged(TrustFailure),
    }

    fn classify(result: i32) -> Verdict {
        if result >= 0 {
            return Verdict::Trusted;
        }
        match result {
            TRUST_E_NOSIGNATURE => Verdict::Unsigned,
            TRUST_E_BAD_DIGEST => Verdict::Damaged(TrustFailure::BadDigest),
            CERT_E_EXPIRED => Verdict::Damaged(TrustFailure::Expired),
            // A revocation lookup that could not complete is not evidence of
            // revocation, and it is not evidence of tampering either: with
            // cache-only retrieval it means this machine holds no cached CRL,
            // which is a property of the machine and not of the artifact.
            CERT_E_REVOKED | CERT_E_REVOCATION_FAILURE => Verdict::Damaged(TrustFailure::Revoked),
            TRUST_E_UNTRUSTEDTESTROOT | CERT_E_UNTRUSTEDROOT | TRUST_E_SUBJECT_NOT_TRUSTED => {
                Verdict::UntrustedChain
            }
            CRYPT_E_FILE_ERROR | TRUST_E_SUBJECT_FORM_UNKNOWN => {
                Verdict::Damaged(TrustFailure::Unsupported)
            }
            other => Verdict::Damaged(TrustFailure::Other(other as u32)),
        }
    }

    /// Which countersignature the blob carries, if any.
    ///
    /// Both OIDs are searched in the cleartext DER of the whole `SignedData`,
    /// which is where the `SignerInfo`'s unauthenticated attributes live.
    fn timestamp_in(blob: &[u8]) -> Timestamp {
        if contains(blob, OID_RFC3161) {
            Timestamp::Rfc3161
        } else if contains(blob, OID_PKIX_COUNTERSIGN) {
            Timestamp::LegacyOnly
        } else {
            Timestamp::None
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack.len() >= needle.len()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    pub(super) fn inspect(path: &Path) -> Result<Authenticode, VerificationError> {
        // The structural questions, read from the file. A certificate table that
        // declares a signature and does not contain one is an error here, not an
        // `Unsigned`, because "unsigned" is the answer a caller ships on.
        let signature =
            zup_pe::embedded_signature(path).map_err(|error| VerificationError::Read {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?;
        if signature.is_none() {
            return Ok(Authenticode::Unsigned);
        }
        let timestamp = match zup_pe::signature_blob(path) {
            Some(blob) => timestamp_in(&blob),
            None => Timestamp::None,
        };
        let verdict = classify(trust(path));
        let Some(certificate) = zup_pe::signing_certificate(path) else {
            // A signature nobody can attribute. Whatever `WinVerifyTrust` said,
            // this file is not something to ship.
            return match verdict {
                Verdict::Unsigned => Ok(Authenticode::Unsigned),
                Verdict::Trusted | Verdict::UntrustedChain | Verdict::Damaged(_) => {
                    Ok(Authenticode::Damaged {
                        identity: None,
                        failure: TrustFailure::Unsupported,
                    })
                }
            };
        };
        let identity = SignerIdentity {
            subject: short_name(&certificate.subject),
            issuer: short_name(&certificate.issuer),
            thumbprint: certificate.fingerprint,
        };
        Ok(match verdict {
            Verdict::Trusted => Authenticode::Signed(SignedImage {
                identity,
                trusted_chain: true,
                timestamp,
            }),
            Verdict::UntrustedChain => Authenticode::Signed(SignedImage {
                identity,
                trusted_chain: false,
                timestamp,
            }),
            Verdict::Unsigned => Authenticode::Unsigned,
            Verdict::Damaged(failure) => Authenticode::Damaged {
                identity: Some(identity),
                failure,
            },
        })
    }

    /// `2.5.4.3=Acme, 2.5.4.10=Zup` as `CN=Acme, O=Zup`.
    ///
    /// The attribute OIDs a subject is almost always made of, with the
    /// conventional short names. Anything else keeps its OID, which is worse than
    /// pretty and better than ambiguous - and `zup-signing`'s publisher policy
    /// matches a fragment, so a name a person can read is a name a person can
    /// configure.
    fn short_name(rendered: &str) -> String {
        rendered
            .split(',')
            .map(|pair| {
                let Some((oid, value)) = pair.split_once('=') else {
                    return pair.trim().to_owned();
                };
                let alias = match oid.trim() {
                    "2.5.4.3" => "CN",
                    "2.5.4.10" => "O",
                    "2.5.4.6" => "C",
                    "2.5.4.7" => "L",
                    "2.5.4.8" => "ST",
                    "2.5.4.11" => "OU",
                    other => other,
                };
                format!("{alias}={value}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use super::{Authenticode, VerificationError};

    pub(super) fn inspect(_path: &Path) -> Result<Authenticode, VerificationError> {
        Err(VerificationError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(subject: &str, thumbprint: &str) -> SignerIdentity {
        SignerIdentity {
            subject: subject.to_owned(),
            issuer: "Test".to_owned(),
            thumbprint: thumbprint.to_owned(),
        }
    }

    fn image(trusted: bool, timestamp: Timestamp) -> SignedImage {
        SignedImage {
            identity: identity("CN=Zup Test", "AABBCC"),
            trusted_chain: trusted,
            timestamp,
        }
    }

    /// The product's policy is the conjunction of three independent demands, and
    /// the development policy differs from it in exactly one of them: it accepts
    /// an untrusted chain. That is the only way a developer signing with a
    /// self-signed certificate gets a usable build without the product policy ever
    /// being relaxed.
    #[test]
    fn the_production_policy_demands_a_trusted_chain_and_an_rfc3161_timestamp() {
        let policy = SignaturePolicy::default();
        assert!(policy.accepts(&image(true, Timestamp::Rfc3161)).is_ok());
        assert!(matches!(
            policy.accepts(&image(false, Timestamp::Rfc3161)),
            Err(VerificationError::UntrustedChain)
        ));
        assert!(matches!(
            policy.accepts(&image(true, Timestamp::None)),
            Err(VerificationError::MissingTimestamp)
        ));
        assert!(matches!(
            policy.accepts(&image(true, Timestamp::LegacyOnly)),
            Err(VerificationError::LegacyTimestamp)
        ));

        let development = SignaturePolicy::test_identity();
        assert!(
            development.accepts(&image(false, Timestamp::None)).is_ok(),
            "only the chain demand is relaxed for a development certificate"
        );
    }

    #[test]
    fn a_publisher_is_matched_by_normalized_subject_and_exact_thumbprint() {
        let policy = SignaturePolicy::test_identity()
            .signed_by("Acme, Inc.")
            .thumbprint("aa:bb:cc");
        assert!(policy.accepts(&image(false, Timestamp::None)).is_err());
        let policy = SignaturePolicy::test_identity()
            .signed_by("Acme, Inc.")
            .thumbprint("AABBCC");
        let matching = SignedImage {
            identity: identity("CN=Acme, Inc. (Acme Inc.), O=Acme, C=GB", "AABBCC"),
            trusted_chain: false,
            timestamp: Timestamp::None,
        };
        assert!(policy.accepts(&matching).is_ok());
    }

    /// The evidence a verified file produces is the portable vocabulary, and
    /// every entry is a public identity or a fact about one. A release
    /// description is published; nothing secret may reach it.
    #[test]
    fn verified_evidence_is_public_and_says_what_was_established() {
        let verified = VerifiedFile {
            path: std::path::PathBuf::from("Acme-Setup.exe"),
            image: image(true, Timestamp::Rfc3161),
        };
        let evidence = verified.evidence();
        assert!(zup_signing::covers_bytes(&evidence));
        assert_eq!(
            zup_signing::timestamp(&evidence),
            Some("rfc3161"),
            "a release description has to be able to say the timestamp is RFC 3161"
        );
        assert_eq!(zup_signing::publisher(&evidence), Some("CN=Zup Test"));
        assert_eq!(zup_signing::thumbprint(&evidence), Some("AABBCC"));
        assert!(
            evidence
                .iter()
                .any(|entry| entry.fact == EvidenceFact::PlatformTrustAccepted),
            "Windows accepted the chain, and the release description should say so"
        );
        let encoded = serde_json::to_string(&evidence).expect("encode");
        for word in ["password", "pfx", "secret", "token", "private"] {
            assert!(!encoded.contains(word), "the evidence mentions `{word}`");
        }
    }
}
