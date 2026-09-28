//! What a toolchain descriptor and a release index must satisfy.
//!
//! Two properties:
//!
//! 1. **Encode/parse identity.** A descriptor read off a machine and a descriptor
//!    re-published from it are the same claim, or a build host and a target
//!    machine disagree about what a component is.
//! 2. **Every path stays inside the release.** `resolve` joins an index path onto
//!    a root and then reads it, so `..` in one is a file outside the release that a
//!    verified index vouched for without ever having read it.
//!
//! The second is stated as a property over the parsed value rather than as a
//! character blacklist, because a blacklist is a claim about the characters
//! somebody thought of and this is a claim about the rule.

use proptest::prelude::*;
use zup_toolchain::{ComponentDescriptor, DESCRIPTOR_SUFFIX, ToolchainRelease};

fn check(data: &[u8]) {
    if let Ok(descriptor) = ComponentDescriptor::parse(data) {
        let encoded = descriptor.encode();
        let reparsed = ComponentDescriptor::parse(encoded.as_bytes())
            .unwrap_or_else(|error| panic!("a re-encoded descriptor must parse: {error}"));
        assert_eq!(
            descriptor, reparsed,
            "the descriptor round trip changed the value"
        );
        assert_eq!(
            encoded,
            reparsed.encode(),
            "the descriptor is not canonically encoded"
        );
    }

    let Ok(index) = ToolchainRelease::parse(data) else {
        return;
    };
    let encoded = index.encode();
    let reparsed = ToolchainRelease::parse(encoded.as_bytes())
        .unwrap_or_else(|error| panic!("a re-encoded index must parse: {error}"));
    assert_eq!(index, reparsed, "the index round trip changed the value");
    assert_eq!(
        encoded,
        reparsed.encode(),
        "the index is not canonically encoded"
    );

    for file in index.files() {
        assert!(!file.path.is_empty(), "the index names an empty path");
        assert!(
            !file.path.contains('\\'),
            "`{}` uses a backslash, so two spellings of one file exist",
            file.path
        );
        assert!(
            !file.path.starts_with('/') && !file.path.contains(':'),
            "`{}` is not relative to the release root",
            file.path
        );
        for segment in file.path.split('/') {
            assert!(
                !segment.is_empty() && segment != "." && segment != "..",
                "`{}` has a segment that leaves the release root",
                file.path
            );
        }
        assert_eq!(
            file.path.contains(DESCRIPTOR_SUFFIX),
            file.path.ends_with(DESCRIPTOR_SUFFIX),
            "`{}` names a descriptor in the middle of a name",
            file.path
        );
    }
    let mut sorted: Vec<&str> = index.files().map(|file| file.path.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        index.paths(),
        sorted,
        "`paths` is not the sorted, deduplicated set"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn toolchain_bytes_hold_the_toolchain_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check(&data);
    }
}
