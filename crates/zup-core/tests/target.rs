use zup_core::{Source, TargetProfile, TargetProfileId, TargetTriple};

#[test]
fn parses_and_canonicalizes_target() {
    let target = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();

    assert_eq!(target.as_str(), "aarch64-pc-windows-msvc");
    assert_eq!(target.architecture().to_string(), "aarch64");
    assert_eq!(target.operating_system().to_string(), "windows");
}

#[test]
fn canonicalizes_x64_and_arm64_aliases() {
    let x64 = TargetTriple::parse("x64-pc-windows-msvc").unwrap();
    let arm64 = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();

    assert_eq!(x64.as_str(), "x86_64-pc-windows-msvc");
    assert_eq!(arm64.as_str(), "aarch64-pc-windows-msvc");
    assert_ne!(x64, arm64);
    assert_eq!(x64, TargetTriple::parse("x86_64-pc-windows-msvc").unwrap());
    assert_eq!(
        arm64,
        TargetTriple::parse("aarch64-pc-windows-msvc").unwrap()
    );
}

#[test]
fn rejects_invalid_target() {
    let error = TargetTriple::parse("not-a-target").unwrap_err();

    assert!(error.to_string().contains("not-a-target"));
}

#[test]
fn rejects_unknown_identity_components() {
    assert!(TargetTriple::parse("unknown-unknown-unknown").is_err());
}

#[test]
fn serde_roundtrip_uses_canonical_target() {
    let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
    let json = serde_json::to_string(&target).unwrap();

    assert_eq!(json, "\"x86_64-pc-windows-msvc\"");
    assert_eq!(serde_json::from_str::<TargetTriple>(&json).unwrap(), target);
}

#[test]
fn validates_target_profile_id() {
    let id = TargetProfileId::new("  windows-x64  ").unwrap();

    assert_eq!(id.as_str(), "windows-x64");
    assert!(TargetProfileId::new(" \t ").is_err());
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"windows-x64\"");
    assert_eq!(
        serde_json::from_str::<TargetProfileId>("\"windows-x64\"").unwrap(),
        id
    );
}

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
