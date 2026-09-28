//! What an artifact index, blob table, variant manifest and release description
//! must satisfy, over generated input.
//!
//! These are the documents a build writes and a downloader or a publisher reads,
//! so a writer and a reader that disagree have no compiler between them: the
//! disagreement shows up as a release no download can satisfy. Each property below
//! is a claim the code could violate without a type error.
//!
//! - **Canonical form is a fixed point.** `encode(parse(encode(x))) == encode(x)`
//!   for the index and the release description, because both are hashed or
//!   published by digest: an encoder that reordered keys between two runs of the
//!   same release would produce two releases that claim to be the same and are
//!   not.
//! - **Ordering survives a round trip.** Validation requires strictly ascending
//!   variant ids and strictly ascending digests, so a parse that could reorder
//!   what it accepted would make both vacuous.
//! - **A manifest agrees with itself.** The platform follows from the target, the
//!   plan is for that target and frontend, and the content digest list is a
//!   deterministic function of the plan.
//! - **The two finalization questions partition the artifacts.** `unsigned` and
//!   `unfinalized` name different artifacts: "unsigned" is a statement about
//!   published bytes, and an artifact with no published bytes has none to make.
//!
//! The bytes are arbitrary, so almost every run exercises only the "did not parse"
//! path. The properties earn their keep in the runs that find an input the parser
//! accepts, which is exactly the input a hostile or buggy peer would send.

use proptest::prelude::*;
use zup_artifact::{ArtifactIndex, BlobTable, RELEASE_SCHEMA, ReleaseManifest, VariantManifest};

/// The artifact index, the blob table, and the variant manifest.
fn check_artifacts(data: &[u8]) {
    if let Ok(index) = ArtifactIndex::parse(data) {
        let encoded = index
            .encode()
            .unwrap_or_else(|error| panic!("a parsed index must re-encode: {error}"));
        let reparsed = ArtifactIndex::parse(&encoded)
            .unwrap_or_else(|error| panic!("a re-encoded index must parse: {error}"));
        assert_eq!(index, reparsed, "the index round trip changed the value");
        assert_eq!(
            encoded,
            reparsed.encode().expect("a re-encoded index re-encodes"),
            "canonical encoding is not a fixed point, so the same release can be published \
             twice with different bytes"
        );
        assert!(
            !index.variants.is_empty(),
            "a parsed index names no variants"
        );
        for pair in index.variants.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "variants are not strictly ascending: {:?} then {:?}",
                pair[0].id,
                pair[1].id
            );
        }
    }

    if let Ok(manifest) = VariantManifest::parse(data) {
        assert_eq!(
            manifest.platform,
            zup_artifact::Platform::from_triple(&manifest.target),
            "a parsed manifest's platform does not follow from its target"
        );
        assert_eq!(
            manifest.plan.installer.target, manifest.target,
            "a parsed manifest's plan is for a different machine"
        );
        assert_eq!(
            manifest.plan.installer.frontend, manifest.frontend,
            "a parsed manifest's plan presents a different experience"
        );
        let digests = manifest.content_digests();
        for pair in digests.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "content digests are not ordered: {digests:?}"
            );
        }
    }

    if let Ok(table) = BlobTable::parse(data) {
        let segments = u32::from(table.segments);
        let entries: Vec<_> = table.entries().copied().collect();
        for pair in entries.windows(2) {
            assert!(
                pair[0].digest < pair[1].digest,
                "a parsed table is not sorted by digest"
            );
        }
        // No "logical size is at least the compressed one" claim: a Zstandard
        // frame is larger than a tiny or incompressible payload, so the table
        // legitimately records a compressed size above the logical one. What the
        // table *does* guarantee is that every entry names a segment it exists
        // in, because every read in the store is addressed by it.
        for entry in &entries {
            if segments > 0 {
                assert!(
                    u32::from(entry.segment) < segments,
                    "a blob names segment {} of {segments}",
                    entry.segment
                );
            }
        }
    }
}

/// The release description: the document that says which bytes were published.
fn check_release(data: &[u8]) {
    let Ok(manifest) = ReleaseManifest::parse(data) else {
        return;
    };
    assert_eq!(
        manifest.schema, RELEASE_SCHEMA,
        "a parsed release description kept an unknown schema"
    );
    let encoded = manifest
        .encode()
        .unwrap_or_else(|error| panic!("a parsed release must re-encode: {error}"));
    let reparsed = ReleaseManifest::parse(&encoded)
        .unwrap_or_else(|error| panic!("a re-encoded release must parse: {error}"));
    assert_eq!(
        manifest, reparsed,
        "the release description round trip changed the value"
    );
    assert_eq!(
        encoded,
        reparsed.encode().expect("a re-encoded release re-encodes"),
        "the release description is not canonically encoded, so the same release publishes \
         differently each time"
    );

    // Ids are the keys every query uses, so two artifacts sharing one is a
    // document nothing can address.
    for pair in manifest.artifacts.windows(2) {
        assert_ne!(pair[0].id, pair[1].id, "two artifacts share an id");
    }

    let unfinalized = manifest.unfinalized();
    let unsigned = manifest.unsigned();
    for artifact in &manifest.artifacts {
        if artifact.finalized.is_none() {
            assert!(
                unfinalized.contains(&artifact.id.as_str()),
                "`unfinalized` does not name {}",
                artifact.id
            );
            assert!(
                !unsigned.contains(&artifact.id.as_str()),
                "`unsigned` named the unfinalized artifact {}",
                artifact.id
            );
            continue;
        }
        assert!(
            !unfinalized.contains(&artifact.id.as_str()),
            "`unfinalized` names the finalized artifact {}",
            artifact.id
        );
        let finalized = artifact.finalized.as_ref().expect("checked above");
        assert_eq!(
            unsigned.contains(&artifact.id.as_str()),
            !finalized.is_signed(),
            "`unsigned` and the recorded evidence disagree about {}",
            artifact.id
        );
    }
    assert_eq!(
        manifest.is_signed(),
        !manifest.artifacts.is_empty()
            && manifest
                .artifacts
                .iter()
                .all(|artifact| artifact.finalized.as_ref().is_some_and(|f| f.is_signed())),
        "`is_signed` does not mean what its parts say"
    );
    assert_eq!(
        manifest.is_finalized(),
        !manifest.artifacts.is_empty() && unfinalized.is_empty(),
        "`is_finalized` does not mean what `unfinalized` says"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn artifact_bytes_hold_the_artifact_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check_artifacts(&data);
    }

    #[test]
    fn release_bytes_hold_the_release_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check_release(&data);
    }
}
