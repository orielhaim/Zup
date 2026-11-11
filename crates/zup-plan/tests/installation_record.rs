mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::collections::BTreeMap;

use zup_core::{
    ActivationRevision, AppId, InstallationId, SelectedScope, TargetPath, TransactionId, hash_bytes,
};
use zup_plan::{
    DeploymentSnapshot, InstallationRecord, InstallationRecordCodecError, InstallationState,
};

fn record() -> InstallationRecord {
    let target = "linux-x64".parse().unwrap();
    {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: TransactionId::new_v7(),
            plan_hash: hash_bytes(b"plan-a"),
        };
        InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: InstallationId::new_v7(),
            state: InstallationState::Installed,
            app_id: AppId::new("example.application").unwrap(),
            scope: SelectedScope::User,
            target,
            directory: TargetPath::new(target, "/opt/application").unwrap(),
            current: DeploymentSnapshot {
                maintenance: None,
                requirements: requirements_fixture::requirements(
                    target,
                    SelectedScope::User,
                    "example.application",
                    "1.0.0",
                ),
                revision: ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: "1.0.0".parse().unwrap(),
                selected_components: vec![],
                release: None,
                preset: None,
                materialization: None,
                resources: BTreeMap::new(),
            },
            retained: vec![],
        }
    }
}

#[test]
fn persisted_record_requires_the_current_schema_and_matching_integrity() {
    let original = record();
    let encoded = original.encode().unwrap();
    assert_eq!(InstallationRecord::decode(&encoded).unwrap(), original);
    let mut altered: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    altered["record"]["current"]["version"] = serde_json::json!("2.0.0");
    assert!(matches!(
        InstallationRecord::decode(&serde_json::to_vec(&altered).unwrap()),
        Err(InstallationRecordCodecError::Integrity)
    ));
    let mut unsupported: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    unsupported["schema"] = serde_json::json!(2);
    assert!(matches!(
        InstallationRecord::decode(&serde_json::to_vec(&unsupported).unwrap()),
        Err(InstallationRecordCodecError::Schema)
    ));
}

#[test]
fn retained_deployments_preserve_their_selection_and_ownership_snapshot() {
    let mut installation = record();
    let path = installation.directory.join("shared.dll").unwrap();
    let key = zup_core::ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
            },
        },
    };
    installation.current.resources.insert(
        key.clone(),
        zup_plan::OwnedResource::File {
            replacement: zup_core::FixedReplacement::Atomic,
            instance: zup_core::FixedFileInstance {
                origin: installation.current.transaction,
                object: zup_core::NativeObjectIdentity::Linux {
                    device: 1,
                    inode: 1,
                },
            },
            destination: path,
            source_relative: zup_core::RelativePath::new("shared.dll").unwrap(),
            sha256: hash_bytes(b"old"),
            size: 3,
            executable: false,
            created_directories: vec![],
            privilege: zup_core::Privilege::User,
        },
    );
    let previous = installation.current.clone();
    installation.retained.push(previous.clone());
    installation.current.revision = ActivationRevision::FIRST.next().unwrap();
    installation.current.transaction = TransactionId::new_v7();
    installation.current.plan_hash = hash_bytes(b"plan-b");
    installation.current.version = "2.0.0".parse().unwrap();
    installation.current.requirements.app.version = installation.current.version.clone();
    if let Some(zup_plan::OwnedResource::File {
        sha256, instance, ..
    }) = installation.current.resources.get_mut(&key)
    {
        *sha256 = hash_bytes(b"new");
        *instance = zup_core::FixedFileInstance {
            origin: installation.current.transaction,
            object: zup_core::NativeObjectIdentity::Linux {
                device: 1,
                inode: 2,
            },
        };
    } else {
        panic!("fixed payload fixture is missing");
    }
    installation
        .current
        .selected_components
        .push(zup_core::ComponentId::new("extra").unwrap());
    installation.current.requirements.components = installation.current.selected_components.clone();
    let persisted = InstallationRecord::decode(&installation.encode().unwrap()).unwrap();
    assert_eq!(persisted.retained, vec![previous]);
    assert_eq!(persisted.current.version.to_string(), "2.0.0");
    assert_eq!(persisted.current.revision.value(), 2);
    let old = zup_core::NativeObjectIdentity::Linux {
        device: 1,
        inode: 1,
    };
    assert!(persisted.references_file_object(&key, old));
    let mut retired = persisted;
    retired.retained.clear();
    assert!(!retired.references_file_object(&key, old));
}

#[test]
fn materialization_namespace_is_bound_to_the_installation_and_instance() {
    let mut installation = record();
    let id = zup_core::MaterializationId::new_v7();
    installation.current.materialization = Some(
        zup_core::MaterializationSpec::new(
            InstallationId::new_v7(),
            id,
            installation
                .directory
                .join(format!("{}/{id}", zup_core::DEPLOYMENTS_DIRECTORY))
                .unwrap(),
            zup_core::GenerationDescriptor::new(installation.target, vec![]).unwrap(),
        )
        .unwrap(),
    );
    assert!(installation.encode().is_err());
}

#[test]
fn repair_acquires_current_generation_and_fixed_payloads_without_retained_versions() {
    let mut installation = record();
    let materialization = |bytes: &[u8]| {
        let id = zup_core::MaterializationId::new_v7();
        zup_core::MaterializationSpec::new(
            installation.id,
            id,
            installation
                .directory
                .join(format!("{}/{id}", zup_core::DEPLOYMENTS_DIRECTORY))
                .unwrap(),
            zup_core::GenerationDescriptor::new(
                installation.target,
                vec![zup_core::GenerationFile {
                    path: zup_core::RelativePath::new("main").unwrap(),
                    size: bytes.len() as u64,
                    sha256: hash_bytes(bytes),
                    executable: true,
                }],
            )
            .unwrap(),
        )
        .unwrap()
    };
    let old = materialization(b"old generation");
    let current = materialization(b"current generation");
    installation.current.materialization = Some(old);
    installation.retained.push(installation.current.clone());
    installation.current.transaction = TransactionId::new_v7();
    installation.current.revision = ActivationRevision::FIRST.next().unwrap();
    installation.current.materialization = Some(current);
    let destination = installation.directory.join("fixed").unwrap();
    installation.current.resources.insert(
        zup_core::ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
                },
            },
        },
        zup_plan::OwnedResource::File {
            instance: zup_core::FixedFileInstance {
                origin: installation.current.transaction,
                object: zup_core::NativeObjectIdentity::Linux {
                    device: 1,
                    inode: 1,
                },
            },
            replacement: zup_core::FixedReplacement::Atomic,
            destination,
            source_relative: zup_core::RelativePath::new("fixed").unwrap(),
            sha256: hash_bytes(b"fixed payload"),
            size: 13,
            executable: false,
            created_directories: vec![],
            privilege: zup_core::Privilege::User,
        },
    );
    installation.validate().unwrap();
    assert_eq!(
        zup_plan::owned_content_digests(&installation),
        std::collections::BTreeSet::from([
            hash_bytes(b"current generation"),
            hash_bytes(b"fixed payload")
        ])
    );
}
