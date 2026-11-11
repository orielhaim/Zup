mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::collections::BTreeSet;

use zup_core::{
    InstallationId, Privilege, RelativePath, ResourceKey, Target, TargetPath, hash_bytes,
};
use zup_plan::{
    DeploymentEffects, FileOperation, FileOperationKind, FilePrecondition, NativeCacheKind,
    NativeCacheOperation, NodeKind, compile_deployment,
};

fn effects() -> DeploymentEffects {
    let target = Target::LINUX_X64;
    let directory = TargetPath::new(target, "/home/user/.local/share/mime").unwrap();
    let destination = directory.join("packages/example.xml").unwrap();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
            },
        },
    };
    let mut effects = DeploymentEffects::new(
        InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(TargetPath::new(target, "/home/user/application").unwrap());
    effects.files.push(FileOperation {
        key: key.clone(),
        source_relative: RelativePath::new("package.xml").unwrap(),
        destination,
        precondition: FilePrecondition::Absent,
        expected_sha256: hash_bytes(b"package"),
        expected_size: 7,
        privilege: Privilege::User,
        kind: FileOperationKind::Create,
        executable: false,
        conflict: None,
        replacement: zup_core::FixedReplacement::Atomic,
    });
    effects.native_caches.push(NativeCacheOperation {
        kind: NativeCacheKind::Mime,
        directory,
        sources: BTreeSet::from([key]),
    });
    effects
}

#[test]
fn source_changes_precede_regeneration_in_both_execution_and_compensation() {
    let input = effects();
    let plan = compile_deployment(&input).unwrap();
    let source = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .unwrap();
    let cache = input.native_caches[0].id();
    for order in [&plan.execution_order, &plan.rollback_order] {
        assert!(
            order.iter().position(|id| id == &source.id).unwrap()
                < order.iter().position(|id| id == &cache).unwrap()
        );
    }
    let mut unsafe_order = plan.clone();
    let source_index = unsafe_order
        .rollback_order
        .iter()
        .position(|id| id == &source.id)
        .unwrap();
    let cache_index = unsafe_order
        .rollback_order
        .iter()
        .position(|id| id == &cache)
        .unwrap();
    unsafe_order.rollback_order.swap(source_index, cache_index);
    assert!(unsafe_order.validate().is_err());
    let mut missing_source = input;
    missing_source.native_caches[0].sources.clear();
    assert!(compile_deployment(&missing_source).is_err());
}

#[test]
fn compensation_regenerates_before_resuming_the_old_service() {
    let mut input = effects();
    let execution = zup_core::ServiceExecution::SystemdDynamicUser;
    let timeouts = zup_core::ServiceTimeouts::for_execution(&execution);
    let configuration = zup_plan::ServiceState::Registration {
        state_directories: Vec::new(),
        native_timeouts: timeouts.native(),
        dependencies: Vec::new(),
        failure: Default::default(),
        execution,
        readiness: Default::default(),
        display_name: "Worker".into(),
        command: Box::new(zup_core::CommandSpec::new(
            TargetPath::new(input.target, "/opt/worker/bin/worker").unwrap(),
            Vec::new(),
        )),
        start: zup_core::ServiceStart::Manual,
    };
    let key = ResourceKey::Service {
        id: zup_core::ServiceId::new("worker").unwrap(),
    };
    input
        .service_controls
        .push(zup_plan::ServiceControlOperation {
            requires_files: Vec::new(),
            registration: zup_core::NativeServiceRegistration::SourceFile,
            intent: zup_plan::ServiceControlIntent::Transition,
            key: key.clone(),
            native_name: "worker.service".into(),
            privilege: Privilege::System,
            before_configuration: configuration.clone(),
            configuration: configuration.clone(),
            owned_configuration: configuration,
            stage: zup_plan::ServiceControlStage::Quiesce,
            before: zup_plan::ServiceTerminalState::Running,
            after: zup_plan::ServiceTerminalState::Stopped,
            manager_generation: "boot:manager".into(),
            before_execution_generation: "execution-1".into(),
            process_scope: zup_plan::ServiceScopeObservation::NativeManager,
            timeouts,
            dependencies: Vec::new(),
        });
    let plan = compile_deployment(&input).unwrap();
    let resume = zup_plan::OperationId::resource("service_quiesce", &key);
    let cache = input.native_caches[0].id();
    assert!(
        plan.rollback_order
            .iter()
            .position(|id| id == &cache)
            .unwrap()
            < plan
                .rollback_order
                .iter()
                .position(|id| id == &resume)
                .unwrap()
    );
}
