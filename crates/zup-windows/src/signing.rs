use std::path::{Path, PathBuf};

use zup_signing::{EvidenceFact, SigningEvidence};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerIdentity {
    pub subject: String,

    pub issuer: String,

    pub thumbprint: String,
}

impl SignerIdentity {
    pub fn subject_contains(&self, fragment: &str) -> bool {
        if fragment.is_empty() {
            return false;
        }
        normalize_subject(&self.subject).contains(&normalize_subject(fragment))
    }

    pub fn thumbprint_matches(&self, expected: &str) -> bool {
        let wanted: String = expected
            .chars()
            .filter(|character| character.is_ascii_hexdigit())
            .map(|character| character.to_ascii_uppercase())
            .collect();
        !wanted.is_empty() && self.thumbprint == wanted
    }
}

fn normalize_subject(subject: &str) -> String {
    subject
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timestamp {
    Rfc3161,

    LegacyOnly,

    None,
}

impl Timestamp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rfc3161 => "rfc3161",
            Self::LegacyOnly => "legacy",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustFailure {
    BadDigest,

    Expired,

    Revoked,

    Unsupported,

    Other(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authenticode {
    Unsigned,

    Signed(SignedImage),

    Damaged {
        identity: Option<SignerIdentity>,

        failure: TrustFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedImage {
    pub identity: SignerIdentity,

    pub trusted_chain: bool,

    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePolicy {
    pub require_trusted_chain: bool,

    pub require_rfc3161_timestamp: bool,

    pub reject_legacy_timestamp: bool,

    pub subject: Option<String>,

    pub thumbprint: Option<String>,

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

    pub fn signed_by(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn thumbprint(mut self, thumbprint: impl Into<String>) -> Self {
        self.thumbprint = Some(thumbprint.into());
        self
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    pub path: PathBuf,

    pub image: SignedImage,
}

impl VerifiedFile {
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

pub fn inspect(path: &Path) -> Result<Authenticode, VerificationError> {
    if !path.is_file() {
        return Err(VerificationError::Unreadable {
            path: path.to_path_buf(),
        });
    }
    platform::inspect(path)
}

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

    const ACTION_GENERIC_VERIFY_V2: Guid = [
        0x6b, 0xc5, 0xaa, 0x00, 0x44, 0xcd, 0xd0, 0x11, 0x8c, 0xc2, 0x00, 0xc0, 0x4f, 0xc2, 0x95,
        0xee,
    ];

    const WTD_UI_NONE: Dword = 2;
    const WTD_REVOKE_NONE: Dword = 0;
    const WTD_CHOICE_FILE: Dword = 1;
    const WTD_STATEACTION_VERIFY: Dword = 1;
    const WTD_STATEACTION_CLOSE: Dword = 2;

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

    const OID_RFC3161: &[u8] = &[
        0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x96, 0x37, 0x03, 0x03, 0x01,
    ];

    const OID_PKIX_COUNTERSIGN: &[u8] = &[
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x06,
    ];

    #[repr(C)]
    struct WinTrustFileInfo {
        cb_struct: Dword,
        pcwsz_file_path: *const u16,
        h_file: Handle,
        pg_known_subject: *const Guid,
    }

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

    enum Verdict {
        Trusted,

        UntrustedChain,

        Unsigned,

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
