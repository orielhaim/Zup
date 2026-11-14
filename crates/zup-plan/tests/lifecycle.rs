mod support;

use std::collections::BTreeMap;
use zup_core::TargetPath;

use zup_core::{
    App, AppId, ComponentId, NonEmptyString, Privilege, RelativePath, ResourceKey, SelectedScope,
    Target, hash_reader,
};
use zup_plan::{
    DesiredFile, DesiredState, FileOperationKind, HostSnapshot, InstallationRecord,
    LifecycleAction, LifecycleError, ObservedFile, ObservedFileState, OwnedResource, RemovalKind,
    plan_lifecycle,
};

fn file(name: &str, contents: &[u8]) -> DesiredFile<TargetPath> {
    let destination = TargetPath::new(
        Target::parse("windows-x64").unwrap(),
        format!(r"C:\ZupLifecycle\{name}"),
    )
    .unwrap();
    DesiredFile {
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::TreeMember {
                    declaration: zup_core::ResourceId::new("fixture").unwrap(),
                    member: zup_core::RelativePath::new(name).unwrap(),
                },
            },
        },
        source_relative: RelativePath::new(name).unwrap(),
        placement: zup_core::PayloadPlacement::Fixed {
            replacement: zup_core::FixedReplacement::Atomic,
            path: destination,
        },
        size: contents.len() as u64,
        sha256: hash_reader(contents).unwrap().1,
        privilege: Privilege::User,
        executable: false,
    }
}

fn make_target(
    version: &str,
    files: Vec<DesiredFile<zup_core::TargetPath>>,
) -> zup_plan::BoundDeployment {
    DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
        generation: None,
        app: App {
            id: AppId::new("com.zup.lifecycle").unwrap(),
            name: NonEmptyString::new("Lifecycle").unwrap(),
            version: version.parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: Target::parse("windows-x64").unwrap(),
        scope: SelectedScope::User,
        install_directory: TargetPath::new(
            Target::parse("windows-x64").unwrap(),
            r"C:\ZupLifecycle",
        )
        .unwrap(),
        selected_components: vec![ComponentId::new("core").unwrap()],
        prerequisites: vec![],
        files,
        launchers: vec![],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_associations: vec![],
        preset: None,
    }
    .bind_fixed()
    .expect("fixed deployment requirements")
}

fn ledger(files: &[DesiredFile<zup_core::TargetPath>]) -> InstallationRecord {
    let mut ledger = support::installed(&make_target("1.0.0", files.to_vec()));
    for file in files {
        ledger.current.resources.insert(
            file.key.clone(),
            OwnedResource::File {
                replacement: zup_core::FixedReplacement::Atomic,
                instance: zup_core::FixedFileInstance {
                    origin: ledger.current.transaction,
                    object: zup_core::NativeObjectIdentity::Windows {
                        volume: 1,
                        file: [1; 16],
                    },
                },
                destination: file.destination().clone(),
                source_relative: file.source_relative.clone(),
                sha256: file.sha256,
                size: file.size,
                created_directories: vec![],
                executable: false,
                privilege: file.privilege,
            },
        );
    }
    ledger
}

fn snapshot(target: &zup_plan::BoundDeployment, states: &[ObservedFileState]) -> HostSnapshot {
    HostSnapshot {
        files: target
            .files
            .iter()
            .zip(states)
            .map(|(file, state)| ObservedFile {
                key: file.key.clone(),
                path: file.destination().clone(),
                state: state.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

#[rstest::rstest]
#[case("1.0.0")]
#[case("2.0.0")]
fn upgrade_adds_updates_and_retires_owned_files(#[case] version: &str) {
    let old_a = file("a.exe", b"old-a");
    let old_b = file("b.exe", b"old-b");
    let ledger = ledger(&[old_a.clone(), old_b.clone()]);
    let target = make_target(
        version,
        vec![file("a.exe", b"new-a"), file("c.exe", b"new-c")],
    );
    let snapshot = snapshot(
        &target,
        &[
            ObservedFileState::File {
                object: zup_core::NativeObjectIdentity::Windows {
                    volume: 1,
                    file: [1; 16],
                },
                executable: None,
                size: old_a.size,
                sha256: old_a.sha256,
            },
            ObservedFileState::Absent,
        ],
    );
    let matches = BTreeMap::from([(old_a.key.clone(), true), (old_b.key.clone(), true)]);
    let plan = plan_lifecycle(
        LifecycleAction::Upgrade,
        Some(&target),
        Some(&snapshot),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Replace);
    assert_eq!(plan.files[1].kind, FileOperationKind::Create);
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(plan.removals[0].key, old_b.key);
    assert_eq!(plan.removals[0].kind, RemovalKind::RemoveOwned);
    assert!(matches!(
        plan_lifecycle(
            LifecycleAction::Upgrade,
            Some(&make_target("0.9.0", vec![])),
            Some(&HostSnapshot::default()),
            Some(&ledger),
            &matches
        ),
        Err(LifecycleError::Downgrade { .. })
    ));
}

#[test]
fn uninstall_skips_drift_and_needs_no_manifest() {
    let a = file("a.exe", b"a");
    let b = file("b.exe", b"b");
    let ledger = ledger(&[a.clone(), b.clone()]);
    let matches = BTreeMap::from([(a.key.clone(), true), (b.key.clone(), false)]);
    let plan = plan_lifecycle(
        LifecycleAction::Uninstall,
        None,
        None,
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert!(plan.transition.is_remove());
    assert_eq!(
        plan.removals
            .iter()
            .filter(|op| op.kind == RemovalKind::RemoveOwned)
            .count(),
        1
    );
    assert_eq!(
        plan.removals
            .iter()
            .filter(|op| op.kind == RemovalKind::Drift)
            .count(),
        1
    );
}

#[test]
fn repair_restores_missing_and_requires_force_for_digest_drift() {
    let a = file("a.exe", b"owned");
    let target = make_target("1.0.0", vec![a.clone()]);
    let ledger = ledger(std::slice::from_ref(&a));
    let matches = BTreeMap::from([(a.key.clone(), false)]);
    let missing = snapshot(&target, &[ObservedFileState::Absent]);
    let plan = plan_lifecycle(
        LifecycleAction::Repair { force: false },
        Some(&target),
        Some(&missing),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::RestoreOwned);
    let changed = snapshot(
        &target,
        &[ObservedFileState::File {
            object: zup_core::NativeObjectIdentity::Windows {
                volume: 1,
                file: [1; 16],
            },
            executable: None,
            size: 4,
            sha256: hash_reader(&b"edit"[..]).unwrap().1,
        }],
    );
    let ordinary = plan_lifecycle(
        LifecycleAction::Repair { force: false },
        Some(&target),
        Some(&changed),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(ordinary.files[0].kind, FileOperationKind::Drift);
    let forced = plan_lifecycle(
        LifecycleAction::Repair { force: true },
        Some(&target),
        Some(&changed),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(forced.files[0].kind, FileOperationKind::RepairOwned);
}

#[test]
fn observations_are_matched_by_identity_for_install_and_repair() {
    let a = file("a.exe", b"a");
    let b = file("b.exe", b"bb");
    let target = make_target("1.0.0", vec![a.clone(), b.clone()]);
    let installed = ledger(&[a.clone(), b.clone()]);
    let mut observed = snapshot(
        &target,
        &[
            ObservedFileState::File {
                object: zup_core::NativeObjectIdentity::Windows {
                    volume: 1,
                    file: [1; 16],
                },
                executable: None,
                size: a.size,
                sha256: a.sha256,
            },
            ObservedFileState::Absent,
        ],
    );
    for action in [
        LifecycleAction::Modify,
        LifecycleAction::Repair { force: false },
    ] {
        let expected = plan_lifecycle(
            action,
            Some(&target),
            Some(&observed),
            Some(&installed),
            &BTreeMap::new(),
        )
        .unwrap();
        observed.files.reverse();
        let reordered = plan_lifecycle(
            action,
            Some(&target),
            Some(&observed),
            Some(&installed),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(expected, reordered);
    }
}

#[test]
fn decisions_remain_bound_to_content_identity_and_generation() {
    let a = file("a.exe", b"a");
    let mut target = make_target("1.0.0", vec![a]);
    let observed = snapshot(&target, &[ObservedFileState::Absent]);
    let execution = plan_lifecycle(
        LifecycleAction::Install,
        Some(&target),
        Some(&observed),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(execution.validate_desired(Some(&target)).is_ok());
    let mut tampered = execution.clone();
    tampered.files[0].privilege = Privilege::System;
    assert!(tampered.validate_desired(Some(&target)).is_err());
    target.files[0].sha256 = zup_core::hash_bytes(b"replacement");
    assert!(execution.validate_desired(Some(&target)).is_err());
    target.files[0].key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
            },
        },
    };
    assert!(execution.validate_desired(Some(&target)).is_err());
    assert!(execution.validate_generation(Some(&ledger(&[]))).is_err());
}

#[rstest::rstest]
#[case(Target::WINDOWS_X64, r"C:\App", r"C:\App\data", r"c:\app\DATA")]
#[case(Target::LINUX_X64, "/opt/app", "/opt/app/data", "/opt/app/data/child")]
fn resolved_resources_cannot_occupy_overlapping_paths(
    #[case] platform: Target,
    #[case] root: &str,
    #[case] first: &str,
    #[case] second: &str,
) {
    let mut target = make_target("1.0.0", vec![file("one", b"a"), file("two", b"b")]);
    target.target = platform;
    target.requirements.target = platform;
    target.install_directory = TargetPath::new(platform, root).unwrap();
    for (file, destination) in target.files.iter_mut().zip([first, second]) {
        file.placement = zup_core::PayloadPlacement::Fixed {
            replacement: zup_core::FixedReplacement::Atomic,
            path: TargetPath::new(platform, destination).unwrap(),
        };
    }
    assert!(matches!(
        target.validate_resolved(),
        Err(zup_plan::ContractError::PathCollision { .. } | zup_plan::ContractError::Duplicate(_))
    ));
}

#[test]
fn summaries_are_derived_and_checked_after_content_changes() {
    let mut target = make_target("1.0.0", vec![file("one", b"a")]);
    assert_eq!(target.summary().unwrap().install_bytes, 1);
    target.files.push(file("two", b"bb"));
    assert_eq!(target.summary().unwrap().install_bytes, 3);
    target.files[0].size = u64::MAX;
    assert!(matches!(
        target.summary(),
        Err(zup_plan::ContractError::SizeOverflow)
    ));
}

#[rstest::rstest]
#[case(b"old", FileOperationKind::NoOp)]
#[case(b"new", FileOperationKind::Replace)]
fn windows_case_changes_preserve_ownership(
    #[case] contents: &[u8],
    #[case] expected: FileOperationKind,
) {
    let mut original = file("APP.exe", b"old");
    original.key = original.key.canonical_for(&Target::WINDOWS_X64);
    let installed = ledger(std::slice::from_ref(&original));
    let mut updated = file("app.exe", contents);
    updated.key = original.key.clone();
    let mut desired = make_target("2.0.0", vec![updated]);
    desired.canonicalize_keys();
    let observed = snapshot(
        &desired,
        &[ObservedFileState::File {
            object: zup_core::NativeObjectIdentity::Windows {
                volume: 1,
                file: [1; 16],
            },
            executable: None,
            size: original.size,
            sha256: original.sha256,
        }],
    );
    let execution = plan_lifecycle(
        LifecycleAction::Upgrade,
        Some(&desired),
        Some(&observed),
        Some(&installed),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(execution.files[0].kind, expected);
    assert!(execution.removals.is_empty());
}

#[test]
fn forced_service_repair_requires_owned_configuration_and_becomes_an_explicit_effect() {
    use zup_core::CommandSpec;
    use zup_plan::{
        DesiredService, IntegrationChangeKind, ObservedService, ObservedServiceState, ServiceState,
    };
    let id = zup_core::ServiceId::new("daemon").unwrap();
    let key = ResourceKey::Service { id: id.clone() };
    let command = CommandSpec {
        executable: file("daemon.exe", b"daemon").destination().clone(),
        arguments: vec!["--serve".into()],
    };
    let mut target = make_target("1.0.0", vec![]);
    target.scope = SelectedScope::Machine;
    target.services.push(DesiredService {
        state_directories: Vec::new(),
        failure: Default::default(),
        execution: zup_core::ServiceExecution::WindowsJob {
            account: zup_core::WindowsServiceAccount::LocalService,
        },
        requires_files: Vec::new(),
        requires_services: Vec::new(),
        dependency_names: Vec::new(),
        lifecycle: zup_core::ServiceLifecycle {
            ..Default::default()
        },
        readiness: Default::default(),
        key: key.clone(),
        id: id.clone(),
        name: NonEmptyString::new("daemon").unwrap(),
        display_name: Some(NonEmptyString::new("Daemon").unwrap()),
        command: command.clone(),
        start: zup_core::ServiceStart::Automatic,
        privilege: Privilege::System,
    });
    target = target.bind_fixed().unwrap();
    let mut owned = ledger(&[]);
    owned.scope = SelectedScope::Machine;
    owned.current.requirements = target.requirements.clone();
    owned.current.resources.insert(
        key.clone(),
        OwnedResource::Service(zup_plan::OwnedService {
            registration: zup_core::NativeServiceRegistration::Scm {
                installation: owned.id,
            },
            timeouts: zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 90,
                stop_observation_seconds: 90,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            name: "daemon".into(),
            privilege: Privilege::System,
            installed: Box::new(ServiceState::Registration {
                state_directories: Vec::new(),
                native_timeouts: zup_core::NativeServiceTimeouts::Scm,
                dependencies: Vec::new(),
                failure: Default::default(),
                execution: zup_core::ServiceExecution::WindowsJob {
                    account: zup_core::WindowsServiceAccount::LocalService,
                },
                readiness: Default::default(),
                display_name: "Daemon".into(),
                command: Box::new(command.clone()),
                start: zup_core::ServiceStart::Automatic,
            }),
        }),
    );
    let snapshot = HostSnapshot {
        services: vec![ObservedService {
            registration: zup_core::ObservedServiceRegistration::Scm {
                claim: zup_core::ServiceOwnershipClaim {
                    installation: owned.id,
                    service: id.clone(),
                },
            },
            key: key.clone(),
            id,
            state: ObservedServiceState::Service {
                state_directories: Vec::new(),
                native_timeouts: zup_core::NativeServiceTimeouts::Scm,
                dependencies: Vec::new(),
                failure: Default::default(),
                execution: zup_core::ServiceExecution::WindowsJob {
                    account: zup_core::WindowsServiceAccount::LocalService,
                },
                readiness: Default::default(),
                display_name: "Administrator edit".into(),
                command: Box::new(command),
                start: zup_core::ServiceStart::Manual,
                runtime_state: None,
            },
        }],
        ..Default::default()
    };
    for (force, expected) in [
        (false, IntegrationChangeKind::Drift),
        (true, IntegrationChangeKind::RepairOwned),
    ] {
        let result = plan_lifecycle(
            LifecycleAction::Repair { force },
            Some(&target),
            Some(&snapshot),
            Some(&owned),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.services[0].kind, expected);
    }
    target.services[0]
        .command
        .arguments
        .push("--different".into());
    target = target.bind_fixed().unwrap();
    assert!(matches!(
        plan_lifecycle(
            LifecycleAction::Repair { force: true },
            Some(&target),
            Some(&snapshot),
            Some(&owned),
            &BTreeMap::new(),
        ),
        Err(zup_plan::LifecycleError::RepairRequirementsChanged)
    ));
}

#[rstest::rstest]
#[case::executable(true, zup_core::FixedReplacement::Atomic)]
#[case::replacement(false, zup_core::FixedReplacement::Quiescent)]
fn repair_cannot_change_committed_file_contracts(
    #[case] executable: bool,
    #[case] policy: zup_core::FixedReplacement,
) {
    let original = file("a.exe", b"owned");
    let installed = ledger(std::slice::from_ref(&original));
    let mut changed = original.clone();
    changed.executable = executable;
    let zup_core::PayloadPlacement::Fixed { replacement, .. } = &mut changed.placement else {
        unreachable!()
    };
    *replacement = policy;
    let target = make_target("1.0.0", vec![changed]);
    let observed = snapshot(
        &target,
        &[ObservedFileState::File {
            object: zup_core::NativeObjectIdentity::Windows {
                volume: 1,
                file: [1; 16],
            },
            size: original.size,
            sha256: original.sha256,
            executable: Some(executable),
        }],
    );
    assert!(matches!(
        plan_lifecycle(
            LifecycleAction::Repair { force: false },
            Some(&target),
            Some(&observed),
            Some(&installed),
            &BTreeMap::new()
        ),
        Err(zup_plan::LifecycleError::RepairRequirementsChanged)
    ));
}
