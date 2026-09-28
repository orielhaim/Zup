//! What a transport package must satisfy, over generated input.
//!
//! The package parser is the one an adapter uses before it has any content, so it
//! runs on everything. Two properties, and the second is the one worth having:
//!
//! 1. **Every rejection is one the code intends.** A *new* rejection cannot be
//!    introduced without somebody reading the list and deciding whether it is a
//!    refusal or a bug; a variant absent from it is a finding, not a detail.
//! 2. **A package that parses can be read end to end.** Every blob the header
//!    names is readable and is the length the header declared, and the index the
//!    package hands out is the one its own bytes parse to. A package that parses
//!    and then cannot be read has two disagreeing implementations of one format.
//!
//! What a property may say is narrow. "Does not panic" is nearly free; the
//! properties worth having are *implications* — a document that parsed re-encodes
//! to itself, a name that was accepted cannot leave a root. A property that
//! restates the parser proves nothing.

use proptest::prelude::*;
use zup_bundle::{Package, PackageError};

/// Every rejection a package parser may legitimately produce.
fn is_expected(error: &PackageError) -> bool {
    matches!(
        error,
        PackageError::Io(_)
            | PackageError::Json(_)
            | PackageError::Invalid
            | PackageError::MetadataTooLarge { .. }
            | PackageError::TooManyBlobs { .. }
            | PackageError::TooManyPluginArtifacts { .. }
            | PackageError::PluginAotTooLarge { .. }
            | PackageError::Allocation { .. }
            | PackageError::Payload(_)
            | PackageError::Missing { .. }
            | PackageError::TargetMismatch { .. }
            | PackageError::PluginArtifact(_)
    )
}

/// Check one input.
fn check(data: &[u8]) {
    // The index parser is the one an adapter uses before it has any content, so
    // it runs on every input.
    if let Err(error) = Package::parse_index(data) {
        assert!(is_expected(&error), "unexpected rejection: {error:?}");
    }
    let package = match Package::parse(data) {
        Ok(package) => package,
        Err(error) => {
            assert!(is_expected(&error), "unexpected rejection: {error:?}");
            return;
        }
    };
    let index = package.index_info();
    assert_eq!(
        index.blob_count(),
        package.blob_count(),
        "the index and the package disagree about how many blobs there are"
    );
    for position in 0..package.blob_count() {
        let compressed = package.compressed_blob(position).unwrap_or_else(|error| {
            panic!("blob {position} is in the table but unreadable: {error}")
        });
        let declared = index
            .compressed_size(position)
            .unwrap_or_else(|| panic!("blob {position} is in the table but not in the index"));
        assert_eq!(
            compressed.len() as u64,
            declared,
            "blob {position} is not the length the header declared"
        );
        // Deliberately no claim that the logical size is at least the compressed
        // one. It is not true: a Zstandard frame has a header and a checksum, so
        // a tiny or incompressible blob is *larger* compressed. An invariant that
        // reads like a compression-ratio assumption and is really a framing
        // detail is worse than none — it fails on correct input and teaches the
        // next reader something false.
        let _ = index.size(position);
    }
    let bytes = package
        .index_bytes()
        .expect("a package's own index must be readable");
    let reparsed = Package::parse_index(&bytes)
        .unwrap_or_else(|error| panic!("a package's own index must parse: {error}"));
    assert_eq!(
        index.blob_count(),
        reparsed.blob_count(),
        "the published index and the parsed index name different blob counts"
    );
    assert_eq!(
        index.index_size(),
        reparsed.index_size(),
        "the published index and the parsed index disagree about their own length"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        // 256 cases is enough: the failures this looks for are structural rather
        // than statistical, and a violation whose defect region is 10^-6 wide is
        // a property about a distribution, not about a parser.
        cases: 256,
        // One shrinking pass, not one per case: a shrinking pass is only useful
        // when something broke.
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn package_bytes_hold_the_package_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check(&data);
    }
}
