mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::collections::BTreeSet;

use zup_core::{
    ActivationRevision, BackendResourceId, GenerationDescriptor, InstallationId, MaterializationId,
    MaterializationSpec, Privilege, ResourceKey, TargetPath,
};
use zup_plan::{
    ActivationSelection, BackendOperation, DeploymentEffects, GenerationIntent,
    GenerationOperation, GenerationPreparation, NodeKind, compile_deployment,
};

fn effects() -> DeploymentEffects {
    let target = "windows-x64".parse().unwrap();
    let installation = InstallationId::new_v7();
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    let directory = TargetPath::new(target, r"C:\application").unwrap();
    effects.install_directory = Some(directory.clone());
    let materialization = MaterializationId::new_v7();
    let id = BackendResourceId::new("native-registration").unwrap();
    let key = ResourceKey::Backend { id: id.clone() };
    effects.backend_operations.push(BackendOperation::apply(
        key.clone(),
        id,
        Privilege::User,
        vec![],
    ));
    effects.generations.push(GenerationIntent {
        sources: std::collections::BTreeMap::new(),
        preparation: GenerationPreparation::Materialize,
        materialization: MaterializationSpec::new(
            installation,
            materialization,
            directory
                .join(format!(
                    "{}/{materialization}",
                    zup_core::DEPLOYMENTS_DIRECTORY
                ))
                .unwrap(),
            GenerationDescriptor::new(target, vec![]).unwrap(),
        )
        .unwrap(),
        publication: zup_plan::GenerationPublication::Application {
            previous: None,
            revision: ActivationRevision::FIRST,
        },
        privilege: Privilege::User,
        exposures: BTreeSet::from([key]),
    });
    effects
}

#[rstest::rstest]
#[case(false, ".zup", false)]
#[case(false, ".ZUP/deployments", false)]
#[case(false, ".zup2", true)]
#[case(true, ".zup", false)]
#[case(true, ".ZUP/deployments", false)]
#[case(true, "", false)]
#[case(true, "data", true)]
fn application_resources_cannot_occupy_deployment_state(
    #[case] persistent: bool,
    #[case] suffix: &str,
    #[case] allowed: bool,
) {
    let mut input = effects();
    let directory = input.install_directory.as_ref().unwrap();
    let path = if suffix.is_empty() {
        directory.clone()
    } else {
        directory.join(suffix).unwrap()
    };
    if persistent {
        let data = zup_core::PersistentDirectory {
            id: zup_core::ResourceId::new("data").unwrap(),
            location: zup_core::PersistentLocation::Path {
                path: path.clone(),
                access: zup_core::PersistentAccess::Owner,
            },
            compatibility: Default::default(),
        };
        input.requirements.data.push(
            data.map_path(|path| zup_core::Template::parse(path.as_str()))
                .unwrap(),
        );
        let intent = zup_plan::PersistentDirectoryIntent {
            installation: input.installation,
            directory: data,
            binding: zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding {
                path,
                writers: vec![],
            }),
            previous: None,
            privilege: Privilege::User,
        };
        input.data.extend(
            [
                zup_plan::PersistentDirectoryStage::Prepare,
                zup_plan::PersistentDirectoryStage::Access,
            ]
            .map(|stage| zup_plan::PersistentDirectoryOperation {
                stage,
                intent: intent.clone(),
            }),
        );
    } else {
        let key = ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::TreeMember {
                    declaration: zup_core::ResourceId::new("fixed").unwrap(),
                    member: zup_core::RelativePath::new("file").unwrap(),
                },
            },
        };
        input.files.push(zup_plan::FileOperation {
            key,
            replacement: zup_core::FixedReplacement::Quiescent,
            kind: zup_plan::FileOperationKind::Create,
            destination: path,
            source_relative: zup_core::RelativePath::new("file").unwrap(),
            precondition: zup_plan::FilePrecondition::Absent,
            expected_sha256: zup_core::hash_bytes(b"file"),
            expected_size: 4,
            privilege: Privilege::User,
            executable: false,
            conflict: None,
        });
    }
    assert_eq!(compile_deployment(&input).is_ok(), allowed);
}

#[test]
fn native_exposure_requires_a_complete_verified_generation_chain() {
    let input = effects();
    let plan = compile_deployment(&input).unwrap();
    let intent = input.generations.first().unwrap();
    let seal = intent.operation_id(GenerationOperation::Seal);
    let mut missing_seal = plan.clone();
    missing_seal.nodes.retain(|node| node.id != seal);
    assert!(missing_seal.validate().is_err());
    let native = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::BackendOperation { .. }))
        .unwrap();
    let mut unguarded_exposure = plan.clone();
    unguarded_exposure
        .dependencies
        .retain(|edge| !(edge.from == seal && edge.to == native.id));
    assert!(unguarded_exposure.validate().is_err());
}

#[test]
fn retained_content_is_verified_without_reconstruction_and_new_content_cannot_overwrite_it() {
    let mut input = effects();
    let intent = input.generations.first_mut().unwrap();
    let previous = Some(ActivationSelection {
        revision: ActivationRevision::FIRST,
        materialization: Some(intent.materialization.reference()),
    });
    intent.publication = zup_plan::GenerationPublication::Application {
        previous,
        revision: ActivationRevision::FIRST.next().unwrap(),
    };
    input.observed = previous;
    input.observed_transaction = Some(zup_core::TransactionId::new_v7());
    assert!(compile_deployment(&input).is_err());
    input.generations.first_mut().unwrap().preparation = GenerationPreparation::ReuseVerified;
    let plan = compile_deployment(&input).unwrap();
    assert!(plan.nodes.iter().any(|node| matches!(
        node.kind,
        NodeKind::Generation {
            operation: GenerationOperation::Verify,
            ..
        }
    )));
    assert!(!plan.nodes.iter().any(|node| matches!(
        node.kind,
        NodeKind::Generation {
            operation: GenerationOperation::Materialize,
            ..
        }
    )));
}

#[test]
fn maintenance_content_is_verified_before_exposure_without_application_activation() {
    let mut input = effects();
    let application = input.generations[0].clone();
    let mut maintenance = application.clone();
    let id = MaterializationId::new_v7();
    maintenance.materialization = MaterializationSpec::new(
        input.installation,
        id,
        input
            .install_directory
            .as_ref()
            .unwrap()
            .join(format!(
                "{}/{id}",
                zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY
            ))
            .unwrap(),
        GenerationDescriptor::new(
            input.target,
            vec![zup_core::GenerationFile {
                path: zup_core::RelativePath::new("maintenance.exe").unwrap(),
                sha256: zup_core::hash_bytes(b"runtime"),
                size: 7,
                executable: true,
            }],
        )
        .unwrap(),
    )
    .unwrap();
    maintenance.publication = zup_plan::GenerationPublication::Maintenance;
    maintenance.sources.insert(
        zup_core::RelativePath::new("maintenance.exe").unwrap(),
        zup_plan::GenerationSource {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Maintenance {
                    artifact: zup_core::MaintenanceArtifact::Runtime,
                },
            },
            source_relative: zup_core::RelativePath::new("runtime-source").unwrap(),
        },
    );
    input.generations.push(maintenance.clone());
    let plan = compile_deployment(&input).unwrap();
    let selection = zup_plan::DeploymentSelection::from_plan(&plan);
    assert_eq!(
        selection.generation.unwrap().materialization,
        application.materialization.id()
    );
    assert_eq!(selection.maintenance.unwrap().materialization, id);
    assert_eq!(
        plan.nodes
            .iter()
            .filter(|node| matches!(
                node.kind,
                NodeKind::Generation {
                    operation: GenerationOperation::Activate,
                    ..
                }
            ))
            .count(),
        1
    );
    let native = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::BackendOperation { .. }))
        .unwrap();
    let mut missing_guard = plan.clone();
    missing_guard.dependencies.retain(|edge| {
        !(edge.from == maintenance.operation_id(GenerationOperation::Seal) && edge.to == native.id)
    });
    assert!(missing_guard.validate().is_err());
    input.generations.remove(0);
    let maintenance_only = compile_deployment(&input).unwrap();
    assert_eq!(
        maintenance_only
            .storage_estimate()
            .additional_payload_bytes(),
        7
    );
    input.generations[0].preparation = GenerationPreparation::ReuseVerified;
    input.generations[0].sources.clear();
    let reuse = compile_deployment(&input).unwrap().storage_estimate();
    assert_eq!(reuse.additional_payload_bytes(), 0);
    assert_eq!(reuse.verified_reuse_bytes, 7);
    assert!(!maintenance_only.nodes.iter().any(|node| matches!(
        node.kind,
        NodeKind::Generation {
            operation: GenerationOperation::Activate,
            ..
        }
    )));
}
