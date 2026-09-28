//! What the acquire-side documents must satisfy.
//!
//! # `RelativeContentPath`
//!
//! The untrusted name every source and every cache resolves against a root. It is
//! given names from a manifest, a catalog, an HTTP response and a release
//! description - none of which the machine wrote. So the property is an
//! implication rather than a character list:
//!
//! > If `parse` accepts a path, then no segment of it can leave the root it is
//! > joined onto.
//!
//! A blacklist is a claim about the characters somebody thought of. This is a
//! claim about the rule, and a new refusal reason has to keep it true.
//!
//! # The release documents
//!
//! A thin artifact is the case where everything is untrusted at once, and each
//! document is the input to a scheduler that will open the paths they name. So:
//!
//! 1. **Round trip.** The installer re-publishes the release description into a
//!    TUF repository, so a parse that loses a field publishes something other
//!    than what it read.
//! 2. **A catalog is a set.** Sorted and deduplicated, whatever order the bytes
//!    arrived in - a repeat would make the scheduler plan two transfers for one
//!    object.

use proptest::prelude::*;
use zup_acquire::{ContentCatalog, RelativeContentPath, ReleaseDescriptor, RuntimeHandoff};

/// Every reason the path rule gives, as one representative each.
///
/// The rule is total: it accepts or it refuses, and it refuses for one of these. A
/// new failure mode cannot be introduced without appearing here.
const REFUSED: &[&str] = &[
    "",
    "/absolute",
    "C:/drive",
    "back\\slash",
    "climb/../out",
    "./here",
    "empty//segment",
    "trailing/",
];

fn check_path(data: &[u8]) {
    // Only well-formed UTF-8 is a name; anything else cannot become a path, and
    // `from_utf8` is the boundary that says so.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(path) = RelativeContentPath::parse(text) else {
        return;
    };
    let again =
        RelativeContentPath::parse(&path.to_string()).expect("an accepted path must parse again");
    assert_eq!(path, again, "accepting a path is not idempotent");
    assert_eq!(
        String::from(path.clone()),
        path.to_string(),
        "the owned and borrowed forms of one path disagree"
    );

    for segment in text.split('/') {
        assert!(!segment.is_empty(), "an accepted path has an empty segment");
        assert!(segment != "." && segment != "..", "an accepted path climbs");
        assert!(
            !segment.contains('\\') && !segment.contains('\0'),
            "an accepted path carries a separator the rule does not model"
        );
        // Windows treats a colon as a drive or a stream, and a name that contains
        // one resolves outside the root it was joined onto.
        assert!(
            !segment.contains(':'),
            "an accepted path names a drive or a stream: {text}"
        );
    }
    assert!(
        !text.starts_with('/'),
        "an accepted path is absolute: {text}"
    );

    for candidate in REFUSED {
        assert!(
            RelativeContentPath::parse(candidate).is_err(),
            "`{candidate}` was accepted"
        );
    }
}

fn check_documents(data: &[u8]) {
    if let Ok(release) = ReleaseDescriptor::parse(data) {
        let encoded = release
            .encode()
            .unwrap_or_else(|error| panic!("a parsed release must re-encode: {error}"));
        let reparsed = ReleaseDescriptor::parse(&encoded)
            .unwrap_or_else(|error| panic!("a re-encoded release must parse: {error}"));
        assert_eq!(
            release, reparsed,
            "the release round trip changed the value"
        );
        assert!(
            release.variants.len() <= ReleaseDescriptor::MAX_VARIANTS,
            "a parsed release carries more variants than the schema allows"
        );
        for pair in release.variants.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "a parsed release's variants are not ascending"
            );
        }
    }

    if let Ok(catalog) = ContentCatalog::parse(data) {
        let encoded = catalog
            .encode()
            .unwrap_or_else(|error| panic!("a parsed catalog must re-encode: {error}"));
        let reparsed = ContentCatalog::parse(&encoded)
            .unwrap_or_else(|error| panic!("a re-encoded catalog must parse: {error}"));
        assert_eq!(
            catalog, reparsed,
            "the catalog round trip changed the value"
        );
        for pair in catalog.blobs.windows(2) {
            assert!(
                pair[0].digest < pair[1].digest,
                "a parsed catalog is not sorted by digest"
            );
        }
    }

    if let Ok(handoff) = RuntimeHandoff::parse(data) {
        let encoded = handoff
            .encode()
            .unwrap_or_else(|error| panic!("a parsed handoff must re-encode: {error}"));
        let reparsed = RuntimeHandoff::parse(&encoded)
            .unwrap_or_else(|error| panic!("a re-encoded handoff must parse: {error}"));
        assert_eq!(
            handoff, reparsed,
            "the handoff round trip changed the value"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn content_path_bytes_hold_the_path_property(data in prop::collection::vec(any::<u8>(), 0..512)) {
        check_path(&data);
    }

    #[test]
    fn acquire_bytes_hold_the_document_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check_documents(&data);
    }
}
