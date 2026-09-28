use rstest::rstest;
use zup_core::{Source, TargetProfile, TargetProfileId, TargetTriple};

/// An alias is rewritten to the triple the toolchain actually names, so a plan
/// built on `arm64-` and one built on `aarch64-` are the same target.
#[rstest]
#[case::arm64("arm64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
#[case::x64("x64-pc-windows-msvc", "x86_64-pc-windows-msvc")]
fn canonicalizes_target_aliases(#[case] alias: &str, #[case] canonical: &str) {
    let target = TargetTriple::parse(alias).unwrap();
    assert_eq!(target.as_str(), canonical);
    assert_eq!(
        target.architecture().to_string(),
        canonical.split('-').next().unwrap()
    );
    assert_eq!(target.operating_system().to_string(), "windows");
    assert_eq!(target, TargetTriple::parse(canonical).unwrap());
}

#[rstest]
#[case::malformed("not-a-target")]
#[case::unknown_components("unknown-unknown-unknown")]
fn rejects_invalid_target(#[case] source: &str) {
    assert!(TargetTriple::parse(source).is_err(), "source: {source}");
}

#[test]
fn validates_target_profile_id() {
    let id = TargetProfileId::new("  windows-x64  ").unwrap();
    assert_eq!(id.as_str(), "windows-x64");
    assert!(TargetProfileId::new(" \t ").is_err());
}

/// Deserialization is a normalization point: an aliased triple on the wire must
/// come back canonical, or two records naming the same target compare unequal.
#[test]
fn target_profile_deserializes_canonical_target() {
    let profile: TargetProfile =
        serde_json::from_str(r#"{"target":"arm64-pc-windows-msvc","source":{"directory":"dist"}}"#)
            .unwrap();

    assert_eq!(
        profile.target,
        TargetTriple::parse("aarch64-pc-windows-msvc").unwrap()
    );
    assert_eq!(profile.source, Source::new("dist".into()).unwrap());
    assert_eq!(profile.frontend, None);
    assert_eq!(profile.install, None);
}
