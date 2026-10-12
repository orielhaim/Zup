//! The `.zupui` format, and what a reader does with a file that is not one.
//!
//! Every malformed case here is one a consumer would otherwise meet as a
//! mysterious failure much later: a package that was truncated in transit, a
//! digest that does not match its bytes, a document naming a target twice. The
//! reader is the only place they can be caught, so the reader is what these
//! tests are about.

use rstest::rstest;
use zup_artifact::ArtifactError;
use zup_artifact::preset::{
    HEADER_BYTES, PresetPackageView, PresetPackageWriter, decode_metadata, encode_metadata,
};
use zup_core::{Sha256Digest, Target};
use zup_preset_protocol::{Capabilities, Capability, PresetDescription};

fn target(name: &str) -> Target {
    Target::parse(name).expect("a valid target")
}

fn description() -> PresetDescription {
    PresetDescription::new("aurora", "1.4.2", serde_json::json!({ "type": "object" }))
        .with_capabilities(Capabilities::new([Capability::Components]))
}

/// One package per supported target, which is the shape a real one has.
fn packed(targets: &[&str]) -> Vec<u8> {
    let mut writer = PresetPackageWriter::new(description()).expect("a valid description");
    for name in targets {
        writer
            .add_binary(
                target(name),
                format!("native preset for {name}").repeat(64).into_bytes(),
            )
            .expect("one binary per target");
    }
    writer
        .finish()
        .expect("the package is written and verified")
}

#[test]
fn a_binary_is_addressable_by_target() {
    let bytes = packed(&["windows-x64", "windows-arm64"]);
    let view = PresetPackageView::open(bytes).expect("a package reads back");
    let windows = view
        .binary_for(&target("windows-x64"))
        .expect("the windows binary is present");
    assert_eq!(
        String::from_utf8_lossy(&windows),
        "native preset for windows-x64".repeat(64)
    );
}

#[test]
fn a_missing_target_names_the_ones_there_are() {
    let view = PresetPackageView::open(packed(&["windows-x64"])).expect("a package");
    let error = view
        .binary_for(&target("windows-arm64"))
        .expect_err("no binary for an unbuilt target");
    let message = error.to_string();
    assert!(message.contains("windows-arm64"), "{message}");
    assert!(
        message.contains("windows-x64"),
        "the message names what the package does carry: {message}"
    );
}

#[test]
fn the_same_target_twice_is_refused() {
    let mut writer = PresetPackageWriter::new(description()).expect("a description");
    writer
        .add_binary(target("windows-x64"), b"one".to_vec())
        .expect("the first binary");
    let error = writer
        .add_binary(target("windows-x64"), b"two".to_vec())
        .expect_err("a second binary for the same target");
    assert!(error.to_string().contains("windows-x64"), "{error}");
}

/// Two targets can be built from the same bytes. The package stores those bytes
/// once and both targets read back, because a table with the same key twice is a
/// table that cannot say which stored bytes belong to which entry.
#[test]
fn two_targets_with_identical_bytes_store_the_binary_once() {
    let mut writer = PresetPackageWriter::new(description()).expect("a valid description");
    let shared = b"one binary, two targets".repeat(64).to_vec();
    writer
        .add_binary(target("windows-x64"), shared.clone())
        .expect("the first target");
    writer
        .add_binary(target("windows-x86"), shared.clone())
        .expect("the second target");
    let bytes = writer
        .finish()
        .expect("the package is written and verified");

    let view = PresetPackageView::open(bytes).expect("a package reads back");
    assert_eq!(view.package().blobs.len(), 1, "stored once");
    assert_eq!(view.package().binaries.len(), 2, "one entry per target");
    for name in ["windows-x64", "windows-x86"] {
        assert_eq!(
            view.binary_for(&target(name)).expect("the binary"),
            shared,
            "both targets read the same bytes"
        );
    }
}

/// A file that is not a package is refused, and the refusal names what the file
/// actually starts with, because the commonest cause is a file of some other
/// format under the wrong name.
#[test]
fn a_file_that_is_not_a_package_is_refused() {
    let error =
        PresetPackageView::open(b"this is a long enough file but it is not a preset".to_vec())
            .expect_err("a file that does not open a package");
    let ArtifactError::PresetMagic { found } = error else {
        panic!("a file that is not a package is not read as one");
    };
    assert_eq!(found, "this", "the refusal names what the file starts with");
}

#[test]
fn a_truncated_package_is_refused() {
    let bytes = packed(&["windows-x64"]);
    let error = PresetPackageView::open(bytes[..bytes.len() - 1].to_vec())
        .expect_err("a package missing its last byte");
    assert!(matches!(
        error,
        zup_artifact::ArtifactError::PresetTruncated { .. }
    ));
}

#[test]
fn a_package_with_bytes_after_its_content_is_refused() {
    let mut bytes = packed(&["windows-x64"]);
    bytes.extend_from_slice(b"extra");
    let error = PresetPackageView::open(bytes).expect_err("a package with trailing bytes");
    assert!(matches!(
        error,
        zup_artifact::ArtifactError::PresetTrailing { .. }
    ));
}

/// An unsupported schema is refused at the header, before anything is read.
#[rstest]
/// An unsupported schema is refused at the header, before anything is read.
#[test]
fn an_unsupported_schema_in_the_header_is_refused() {
    let mut bytes = packed(&["windows-x64"]);
    bytes[4..8].copy_from_slice(&9u32.to_le_bytes());
    let error = PresetPackageView::open(bytes).expect_err("an unsupported schema");
    assert!(
        matches!(
            error,
            zup_artifact::ArtifactError::PresetSchema { found: 9, .. }
        ),
        "{error}"
    );
}

/// A document that disagrees with its own header is refused, because a reader
/// that trusted either one of them would be trusting something nobody wrote.
#[test]
fn a_document_disagreeing_with_its_header_is_refused() {
    let bytes = packed(&["windows-x64"]);
    let mut package = decode_metadata(&metadata_of(&bytes)).expect("a document");
    package.schema = 9;
    let document = encode_metadata(&package).expect("the document encodes");
    assert!(matches!(
        decode_metadata(&document),
        Err(zup_artifact::ArtifactError::PresetSchema { found: 9, .. })
    ));
}

/// A binary whose stored bytes changed is refused, and the message names the
/// target and the digest it was supposed to be.
#[test]
fn a_corrupt_blob_is_refused() {
    let mut bytes = packed(&["windows-x64"]);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let view = PresetPackageView::open(bytes).expect("the structure is intact");
    let error = view
        .verify()
        .expect_err("a binary whose bytes changed does not verify");
    let message = error.to_string();
    assert!(message.contains("windows-x64"), "{message}");
    assert!(message.contains("damaged"), "{message}");
}

/// A document that claims the wrong length for a binary is refused on the
/// length, so a truncation and a substitution are told apart rather than both
/// becoming "does not verify".
#[test]
fn a_binary_of_the_wrong_size_is_refused() {
    let bytes = packed(&["windows-x64"]);
    let mut package = decode_metadata(&metadata_of(&bytes)).expect("a document");
    package.binaries[0].size += 1;
    let document = encode_metadata(&package).expect("the document encodes");
    // The document's own length is in the header, so the package is rebuilt
    // around the altered one rather than spliced in.
    let spliced = splice_metadata(&bytes, &document);
    let view = PresetPackageView::open(spliced).expect("the structure is intact");
    let error = view
        .binary_for(&target("windows-x64"))
        .expect_err("a binary that is not the length the document claims");
    assert!(
        matches!(error, zup_artifact::ArtifactError::SizeMismatch { .. }),
        "{error}"
    );
}

/// A document that claims a digest no stored binary has is refused before
/// anything is decompressed, because there is nothing to decompress.
/// A document that claims a digest no stored binary has is refused before
/// anything is decompressed, because there is nothing to decompress.
#[test]
fn a_document_claiming_an_absent_digest_is_refused() {
    let bytes = packed(&["windows-x64"]);
    let mut package = decode_metadata(&metadata_of(&bytes)).expect("a document");
    let absent = package.blobs[0].digest;
    package.blobs[0].digest = Sha256Digest::from_bytes([0u8; 32]);
    package.binaries[0].sha256 = absent;
    let document = encode_metadata(&package).expect("the document encodes");
    assert!(matches!(
        decode_metadata(&document),
        Err(zup_artifact::ArtifactError::PresetMetadata(_))
    ));
}

#[test]
fn a_document_naming_a_binary_it_does_not_store_is_refused() {
    let mut package = decode_metadata(&metadata_of(&packed(&["windows-x64"]))).expect("a document");
    package.blobs.clear();
    let document = encode_metadata(&package).expect("the document encodes");
    assert!(
        matches!(
            decode_metadata(&document),
            Err(zup_artifact::ArtifactError::PresetMetadata(_))
        ),
        "a binary nothing stores is a package that cannot produce a preset"
    );
}

#[test]
fn a_document_with_two_entries_for_one_digest_is_refused() {
    let bytes = packed(&["windows-x64"]);
    let mut package = decode_metadata(&metadata_of(&bytes)).expect("a document");
    let duplicate = package.blobs[0].clone();
    package.blobs.push(duplicate);
    let document = encode_metadata(&package).expect("the document encodes");
    assert!(matches!(
        decode_metadata(&document),
        Err(zup_artifact::ArtifactError::PresetMetadata(_))
    ));
}

#[test]
fn a_settings_schema_that_is_not_an_object_is_refused() {
    let description = PresetDescription::new("aurora", "1.0.0", serde_json::json!("not a schema"));
    assert!(
        PresetPackageWriter::new(description).is_err(),
        "a preset whose settings are not a document cannot be packaged"
    );
}

#[test]
fn a_version_this_package_cannot_state_is_refused() {
    let description = PresetDescription::new("aurora", "not-a-version", serde_json::json!({}));
    let mut writer = PresetPackageWriter::new(description).expect("the description itself is fine");
    writer
        .add_binary(target("windows-x64"), b"bytes".to_vec())
        .expect("a binary");
    assert!(
        writer.finish().is_err(),
        "a preset that does not state a semantic version is not publishable"
    );
}

/// The bytes that make up a package's metadata document.
fn metadata_of(bytes: &[u8]) -> Vec<u8> {
    let metadata_bytes = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
    bytes[HEADER_BYTES..HEADER_BYTES + metadata_bytes as usize].to_vec()
}

/// The same package with a different metadata document.
///
/// The header's metadata length is rewritten, because a document spliced in
/// without it is a package whose header describes a different document.
fn splice_metadata(bytes: &[u8], document: &[u8]) -> Vec<u8> {
    let original = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")) as usize;
    let mut out = Vec::with_capacity(bytes.len() + document.len());
    out.extend_from_slice(&bytes[0..8]);
    out.extend_from_slice(&(document.len() as u64).to_le_bytes());
    out.extend_from_slice(&bytes[16..HEADER_BYTES]);
    out.extend_from_slice(document);
    out.extend_from_slice(&bytes[HEADER_BYTES + original..]);
    out
}
