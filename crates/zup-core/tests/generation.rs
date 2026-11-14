use rstest::rstest;
use zup_core::{GenerationDescriptor, GenerationError, GenerationFile, RelativePath, hash_bytes};

fn file(path: &str) -> GenerationFile {
    GenerationFile {
        path: RelativePath::new(path).unwrap(),
        size: 3,
        sha256: hash_bytes(b"abc"),
        executable: true,
    }
}

#[test]
fn generation_identity_has_a_stable_versioned_hash_preimage() {
    let descriptor =
        GenerationDescriptor::new("linux-x64-gnu".parse().unwrap(), vec![file("app")]).unwrap();
    assert_eq!(
        descriptor.tree_id().digest().to_hex(),
        "490b03d4be461f6dc388754bae5ad0a31435db1106157e68e3245712fbaf2e28"
    );
    let wire = serde_json::to_value(&descriptor).unwrap();
    let decoded: GenerationDescriptor = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded, descriptor);
    assert_eq!(decoded.size(), 3);
}

#[test]
fn enumeration_order_does_not_change_content_identity_but_content_and_mode_do() {
    let target = "linux-x64-gnu".parse().unwrap();
    let original = GenerationDescriptor::new(target, vec![file("app"), file("lib/data")]).unwrap();
    let reordered = GenerationDescriptor::new(target, vec![file("lib/data"), file("app")]).unwrap();
    assert_eq!(original.tree_id(), reordered.tree_id());
    let mut content = original.files().to_vec();
    content[0].sha256 = hash_bytes(b"def");
    assert_ne!(
        original.tree_id(),
        GenerationDescriptor::new(target, content)
            .unwrap()
            .tree_id()
    );
    let mut mode = original.files().to_vec();
    mode[0].executable = false;
    assert_ne!(
        original.tree_id(),
        GenerationDescriptor::new(target, mode).unwrap().tree_id()
    );
}

#[rstest]
#[case("linux-x64-gnu", "bin/app", "bin/app")]
#[case("windows-x64", "bin/App.exe", "BIN/app.exe")]
#[case("linux-x64-gnu", "bin", "bin/app")]
#[case("linux-x64-gnu", "bin/app", "bin")]
fn conflicting_generation_members_are_rejected_even_when_deserialized(
    #[case] target: &str,
    #[case] first: &str,
    #[case] second: &str,
) {
    let files = vec![file(first), file(second)];
    assert!(matches!(
        GenerationDescriptor::new(target.parse().unwrap(), files.clone()),
        Err(GenerationError::Conflict { .. })
    ));
    let untrusted = serde_json::json!({"schema": 1, "target": target, "files": files});
    assert!(serde_json::from_value::<GenerationDescriptor>(untrusted).is_err());
}

#[test]
fn size_overflow_cannot_be_hidden_in_a_generation_descriptor() {
    let mut huge = file("huge");
    huge.size = u64::MAX;
    assert!(matches!(
        GenerationDescriptor::new("linux-x64-gnu".parse().unwrap(), vec![huge, file("extra")]),
        Err(GenerationError::SizeOverflow)
    ));
}
