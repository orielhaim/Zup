use zup_core::{
    ActivationRevision, DeploymentIdentityError, GenerationDescriptor, InstallationId,
    MaterializationId, MaterializationSpec, TargetPath,
};

#[test]
fn deployment_identity_boundaries_reject_nil_and_revision_wraparound() {
    let nil = serde_json::json!("00000000-0000-0000-0000-000000000000");
    assert!(serde_json::from_value::<InstallationId>(nil.clone()).is_err());
    assert!(serde_json::from_value::<MaterializationId>(nil).is_err());
    assert!(serde_json::from_value::<ActivationRevision>(serde_json::json!(0)).is_err());
    assert_eq!(ActivationRevision::FIRST.next().unwrap().value(), 2);
    assert!(matches!(
        ActivationRevision::new(u64::MAX).unwrap().next(),
        Err(DeploymentIdentityError::RevisionExhausted)
    ));
}

#[test]
fn repair_materializations_share_content_identity_and_keep_distinct_physical_identity() {
    let target = "linux-x64".parse().unwrap();
    let descriptor = GenerationDescriptor::new(target, vec![]).unwrap();
    let installation = InstallationId::new_v7();
    let first = MaterializationSpec::new(
        installation,
        MaterializationId::new_v7(),
        TargetPath::new(target, "/application/generations/first").unwrap(),
        descriptor.clone(),
    )
    .unwrap();
    let repaired = MaterializationSpec::new(
        installation,
        MaterializationId::new_v7(),
        TargetPath::new(target, "/application/generations/repaired").unwrap(),
        descriptor,
    )
    .unwrap();
    assert_eq!(first.reference().tree, repaired.reference().tree);
    assert_ne!(first.reference().id, repaired.reference().id);
    let mut untrusted = serde_json::to_value(&repaired).unwrap();
    untrusted["directory"] = serde_json::to_value(
        TargetPath::new(
            "windows-x64".parse::<zup_core::Target>().unwrap(),
            r"C:\foreign",
        )
        .unwrap(),
    )
    .unwrap();
    assert!(serde_json::from_value::<MaterializationSpec>(untrusted).is_err());
}
