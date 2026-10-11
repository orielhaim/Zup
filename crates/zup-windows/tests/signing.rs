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

        assert!(
            matches!(
                verify(&fixture(name), &SignaturePolicy::default()),
                Err(VerificationError::UntrustedChain)
            ),
            "{name}: the production policy demands a trusted chain"
        );

        assert!(
            verify(&fixture(name), &SignaturePolicy::test_identity()).is_ok(),
            "{name}: a development certificate is a real signature"
        );
    }
}

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
