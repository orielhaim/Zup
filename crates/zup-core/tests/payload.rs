use rstest::rstest;
use zup_core::{
    GenerationDescriptor, PayloadId, PayloadPlacement, PayloadRequirement, RelativePath,
    ResourceId, Template, hash_bytes,
};

#[rstest]
#[case("")]
#[case("../foreign")]
#[case("C:\\foreign")]
#[case("two words")]
fn physical_path_syntax_cannot_be_used_as_a_resource_declaration(#[case] value: &str) {
    assert!(ResourceId::new(value).is_err());
    assert!(serde_json::from_value::<ResourceId>(serde_json::json!(value)).is_err());
}

#[test]
fn generation_content_is_selected_by_placement_without_hashing_fixed_paths() {
    let id = PayloadId::TreeMember {
        declaration: ResourceId::new("program").unwrap(),
        member: RelativePath::new("app").unwrap(),
    };
    let code = PayloadRequirement {
        id: id.clone(),
        source_relative: RelativePath::new("build/app").unwrap(),
        placement: PayloadPlacement::Generation {
            path: RelativePath::new("bin/app").unwrap(),
        },
        size: 3,
        sha256: hash_bytes(b"abc"),
        executable: true,
    };
    let mut fixed = PayloadRequirement {
        id: PayloadId::File {
            declaration: ResourceId::new("fixed-data").unwrap(),
        },
        source_relative: RelativePath::new("data").unwrap(),
        placement: PayloadPlacement::Fixed {
            replacement: zup_core::FixedReplacement::Atomic,
            path: Template::parse("/opt/application/data").unwrap(),
        },
        size: 6,
        sha256: hash_bytes(b"fixed!"),
        executable: false,
    };
    let target = "linux-x64".parse().unwrap();
    let descriptor =
        GenerationDescriptor::from_payloads(target, &[code.clone(), fixed.clone()]).unwrap();
    assert_eq!(descriptor.size(), 3);
    assert_eq!(descriptor.files().len(), 1);
    assert_eq!(descriptor.files()[0].path.as_str(), "bin/app");
    fixed.placement = PayloadPlacement::Fixed {
        replacement: zup_core::FixedReplacement::Atomic,
        path: Template::parse("/srv/application/data").unwrap(),
    };
    assert_eq!(
        descriptor.tree_id(),
        GenerationDescriptor::from_payloads(target, &[fixed, code])
            .unwrap()
            .tree_id()
    );
}

#[test]
fn authored_payload_mapping_defaults_to_a_generation_root() {
    let mapping: zup_core::FileMapping =
        serde_json::from_value(serde_json::json!({"id": "program", "source": "**/*"})).unwrap();
    assert!(matches!(
        mapping.placement,
        zup_core::PayloadMapping::Generation { directory: None }
    ));
    assert_eq!(mapping.id.as_str(), "program");
    assert_eq!(
        mapping.member_placement(&RelativePath::new("bin/app").unwrap()),
        PayloadPlacement::Generation {
            path: RelativePath::new("bin/app").unwrap()
        },
    );
    assert!(
        serde_json::from_value::<zup_core::FileMapping>(serde_json::json!({
            "id": "program", "source": "**/*", "destination": "${install}"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<PayloadPlacement>(serde_json::json!({"kind":"fixed"})).is_err()
    );
}
