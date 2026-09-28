//! Windows trust verification, against real signed images.
//!
//! The fixtures in `zup-pe/tests/fixtures` are genuine PE images produced by the
//! Rust toolchain and signed by `Set-AuthenticodeSignature` with a throwaway
//! self-signed certificate. They are the development case exactly: the chain
//! does not reach a root Windows trusts, so the *chain* is refused while the
//! *digest* passes perfectly. A test that required a production certificate
//! would be a test that never runs.
//!
//! What these tests are for is the distinction the whole design rests on:
//!
//! - [`zup_pe`] can say the Authenticode digest covers these bytes, on any host,
//!   because that is a property of the bytes.
//! - This module can say whether *Windows* trusts the chain, which is a fact
//!   about the verifying machine's certificate store and not about the file.
//!
//! A release pipeline needs the second and cannot have it any other way, which
//! is why `WinVerifyTrust` lives here and not in the PE parser.
//!
//! # The fixtures
//!
//! `signed.exe` and `timestamped.exe` are a Rust-built image signed with and
//! without an RFC 3161 countersignature, and `unsigned.exe` is the same build
//! before signing. They are a few hundred kilobytes rather than a hand-written
//! 1.5 KiB stub because the Authenticode subject interface package *refuses* a
//! minimal stub: it rejected the hand-built image with "the form specified for
//! the subject is not one supported or known by the specified trust provider",
//! while accepting the same image produced by a real linker. A fixture the
//! platform will not read cannot test the platform.

use std::path::{Path, PathBuf};

use zup_windows::signing::{
    Authenticode, SignaturePolicy, Timestamp, TrustFailure, VerificationError, inspect, verify,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("two levels below the root")
        .join("crates")
        .join("zup-pe")
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// A signature from a self-signed certificate is a *sound* signature from an
/// untrusted chain, and the two facts have to be reported separately.
///
/// `WinVerifyTrust` answers both halves in one call - it recomputes the message
/// digest and builds the chain - so this is the only place a caller sees them
/// arrive together and has to record them apart. `trusted_chain: false` with the
/// image still `Signed` is exactly the case a single boolean would mishandle, and
/// exactly the case a developer hits on day one.
#[test]
fn a_self_signed_signature_is_signed_but_its_chain_is_not_trusted() {
    for name in ["signed.exe", "timestamped.exe"] {
        let image = inspect(&fixture(name)).expect("a real signed image is readable");
        let signed = match image {
            Authenticode::Signed(image) => image,
            other => panic!("{name} should be Signed, got {other:?}"),
        };
        assert!(!signed.trusted_chain, "{name}: a self-signed chain");
        assert!(
            signed.identity.subject.contains("Zup Test Fixture")
                || signed.identity.subject.contains("Timestamp Responder"),
            "{name}: the subject is readable, got `{}`",
            signed.identity.subject
        );
        assert!(
            !signed.identity.thumbprint.is_empty(),
            "{name}: the certificate has a thumbprint"
        );
        // The production policy refuses it, and it refuses for the *chain*
        // rather than for anything about the bytes.
        assert!(
            matches!(
                verify(&fixture(name), &SignaturePolicy::default()),
                Err(VerificationError::UntrustedChain)
            ),
            "{name}: the production policy demands a trusted chain"
        );
        // The development policy accepts it, which is what makes a local
        // self-signed certificate usable at all.
        assert!(
            verify(&fixture(name), &SignaturePolicy::test_identity()).is_ok(),
            "{name}: a development certificate is a real signature"
        );
    }
}

/// The timestamp is a fact about the signature that is independent of the chain,
/// and the states have to stay distinguishable. This is where current Windows
/// guidance earns its keep: a SHA-256 signature with no timestamp stops
/// validating when the certificate expires, and a policy that cannot tell "no
/// timestamp" from "a legacy timestamp" is a policy that cannot enforce
/// anything.
#[test]
fn the_timestamp_is_reported_separately_from_the_chain() {
    let untimed = match inspect(&fixture("signed.exe")).expect("readable") {
        Authenticode::Signed(image) => image,
        other => panic!("expected Signed, got {other:?}"),
    };
    let timestamped = match inspect(&fixture("timestamped.exe")).expect("readable") {
        Authenticode::Signed(image) => image,
        other => panic!("expected Signed, got {other:?}"),
    };

    // The countersignature is detected by its OID in the PKCS#7, which proves
    // *presence* and nothing more. These two fixtures are the only way to show
    // the detection distinguishes anything at all.
    assert_ne!(
        untimed.timestamp, timestamped.timestamp,
        "a signature with and without a timestamp must not read the same"
    );
    assert!(
        matches!(
            timestamped.timestamp,
            Timestamp::Rfc3161 | Timestamp::LegacyOnly
        ),
        "the countersigned fixture carries a timestamp, got {:?}",
        timestamped.timestamp
    );

    // The production policy's two timestamp rules are distinguishable: the
    // untimed signature is refused for the timestamp, which is a different
    // refusal from the untrusted chain and names a different fix.
    let chain_only = SignaturePolicy {
        require_trusted_chain: false,
        require_rfc3161_timestamp: false,
        reject_legacy_timestamp: false,
        ..SignaturePolicy::default()
    };
    assert!(
        verify(&fixture("signed.exe"), &chain_only).is_ok(),
        "a policy that asks only about the chain accepts an untimed signature"
    );
    let timestamp_required = SignaturePolicy {
        require_trusted_chain: false,
        require_rfc3161_timestamp: true,
        reject_legacy_timestamp: true,
        ..SignaturePolicy::default()
    };
    assert!(
        matches!(
            verify(&fixture("signed.exe"), &timestamp_required),
            Err(VerificationError::LegacyTimestamp | VerificationError::MissingTimestamp)
        ),
        "and a policy that asks about the timestamp refuses it by name"
    );
}

/// A file with no signature is `Unsigned`, and a caller must be able to tell that
/// from a file whose signature is *wrong*. The first is "nobody signed this" and
/// the second is "somebody signed this and it does not verify", and they lead to
/// opposite decisions.
#[test]
fn an_unsigned_image_says_so_rather_than_guessing() {
    let path = fixture("unsigned.exe");
    assert!(matches!(
        inspect(&path).expect("readable"),
        Authenticode::Unsigned
    ));
    assert!(matches!(
        verify(&path, &SignaturePolicy::default()),
        Err(VerificationError::Unsigned(_))
    ));
    assert!(matches!(
        verify(
            &path.parent().expect("a parent").join("absent.exe"),
            &SignaturePolicy::default()
        ),
        Err(VerificationError::Unreadable { .. })
    ));
}

/// A file whose bytes changed after signing is `Damaged { BadDigest }`, and it is
/// the one failure that is a statement about the artifact rather than about
/// trust. `TRUST_E_BAD_DIGEST` is what `WinVerifyTrust` answers, and it must not
/// be folded into the untrusted-chain case: a user can fix an untrusted chain by
/// installing a root, and cannot fix a bad digest by installing anything.
#[test]
fn bytes_that_changed_after_signing_are_reported_as_a_bad_digest() {
    let path = scratch("tampered.exe", &tampered());
    match inspect(&path).expect("a tampered image is still readable") {
        Authenticode::Damaged {
            failure: TrustFailure::BadDigest,
            identity: Some(identity),
        } => {
            assert!(
                identity.subject.contains("Zup Test Fixture"),
                "the signer is still readable, got `{}`",
                identity.subject
            );
        }
        other => panic!("a tampered image is Damaged(BadDigest), got {other:?}"),
    }
    assert!(matches!(
        verify(&path, &SignaturePolicy::test_identity()),
        Err(VerificationError::Refused {
            failure: TrustFailure::BadDigest,
            ..
        })
    ));
}

/// The evidence a verified file produces is what a release description records,
/// and it has to name the publisher and the timestamp - because those are the
/// two facts a downloader acts on and a digest cannot express.
#[test]
fn verified_evidence_records_the_publisher_and_the_timestamp() {
    let verified = verify(
        &fixture("timestamped.exe"),
        &SignaturePolicy::test_identity(),
    )
    .expect("a development certificate verifies");
    assert!(!verified.image.trusted_chain);
    let evidence = verified.evidence();
    assert_eq!(
        zup_signing::publisher(&evidence),
        Some(verified.image.identity.subject.as_str()),
        "a release description has to name the publisher"
    );
    assert!(
        zup_signing::timestamp(&evidence).is_some(),
        "and the timestamp that keeps the signature valid past expiry"
    );
    assert!(
        !evidence
            .iter()
            .any(|entry| entry.fact == zup_signing::EvidenceFact::PlatformTrustAccepted),
        "an untrusted chain is not recorded as a trusted one"
    );
}

/// A byte flipped inside the image, past the headers and before the certificate
/// table, so the signature structure survives and only the covered bytes differ.
fn tampered() -> Vec<u8> {
    let path = fixture("signed.exe");
    let mut bytes = std::fs::read(&path).expect("read");
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    let section =
        pe + 24 + u16::from_le_bytes(bytes[pe + 20..pe + 22].try_into().unwrap()) as usize;
    let first = u32::from_le_bytes(bytes[section + 20..section + 24].try_into().unwrap()) as usize;
    assert!(
        first > 0x200 && first < bytes.len() - 4096,
        "the flip lands in section data, not in the headers or the certificate table"
    );
    bytes[first] ^= 0xff;
    bytes
}

/// Write bytes to a scratch file and return its path, in the target directory
/// rather than the system temp directory, because it is a build artefact.
fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("two levels below the root")
        .join("target")
        .join("signing-fixtures");
    std::fs::create_dir_all(&directory).expect("create");
    let path = directory.join(name);
    std::fs::write(&path, bytes).expect("write");
    path
}
