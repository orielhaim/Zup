use std::collections::BTreeMap;

use zup_core::{
    App, AppId, ComponentId, NonEmptyString, Privilege, RelativePath, ResourceKey, SelectedScope,
    TargetTriple, hash_reader,
};
use zup_exec::{
    FileOperationKind, HostSnapshot, InstallLedger, LifecycleAction, LifecycleError, ObservedFile,
    ObservedFileState, OwnedResource, RemovalKind, plan_lifecycle,
};
use zup_platform::{TargetFile, TargetPath, TargetPlan, TargetPlanSummary};

fn file(name: &str, contents: &[u8]) -> TargetFile {
    let destination = TargetPath::new(
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        format!(r"C:\ZupLifecycle\{name}"),
    )
    .unwrap();
    TargetFile {
        key: ResourceKey::File {
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new(name).unwrap(),
        destination,
        size: contents.len() as u64,
        sha256: hash_reader(contents).unwrap().1,
        privilege: Privilege::User,
    }
}

fn make_target(version: &str, files: Vec<TargetFile>) -> TargetPlan {
    TargetPlan {
        app: App {
            id: AppId::new("com.zup.lifecycle").unwrap(),
            name: NonEmptyString::new("Lifecycle").unwrap(),
            version: version.parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        scope: SelectedScope::User,
        install_directory: TargetPath::new(
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
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
        summary: TargetPlanSummary {
            file_count: 0,
            install_bytes: 0,
            resource_count: 0,
            requires_authorization: false,
            selected_component_count: 1,
            prerequisite_count: 0,
            download_bytes: 0,
        },
    }
}

fn ledger(files: &[TargetFile]) -> InstallLedger {
    let mut ledger = InstallLedger::new(
        AppId::new("com.zup.lifecycle").unwrap(),
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        SelectedScope::User,
    );
    ledger.version = "1.0.0".parse().unwrap();
    ledger.selected_components = vec![ComponentId::new("core").unwrap()];
    for file in files {
        ledger.resources.insert(
            file.key.clone(),
            OwnedResource::File {
                destination: file.destination.clone(),
                source_relative: file.source_relative.clone(),
                sha256: file.sha256,
                size: file.size,
                created_directories: vec![],
                privilege: file.privilege,
            },
        );
    }
    ledger
}

fn snapshot(target: &TargetPlan, states: &[ObservedFileState]) -> HostSnapshot {
    HostSnapshot {
        files: target
            .files
            .iter()
            .zip(states)
            .map(|(file, state)| ObservedFile {
                key: file.key.clone(),
                path: file.destination.clone(),
                state: state.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn upgrade_adds_updates_and_retires_owned_files() {
    let old_a = file("a.exe", b"old-a");
    let old_b = file("b.exe", b"old-b");
    let ledger = ledger(&[old_a.clone(), old_b.clone()]);
    let target = make_target(
        "2.0.0",
        vec![file("a.exe", b"new-a"), file("c.exe", b"new-c")],
    );
    let snapshot = snapshot(
        &target,
        &[
            ObservedFileState::File {
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
    assert!(plan.uninstall);
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
        LifecycleAction::Repair { force_files: false },
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
            size: 4,
            sha256: hash_reader(&b"edit"[..]).unwrap().1,
        }],
    );
    let ordinary = plan_lifecycle(
        LifecycleAction::Repair { force_files: false },
        Some(&target),
        Some(&changed),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(ordinary.files[0].kind, FileOperationKind::Drift);
    let forced = plan_lifecycle(
        LifecycleAction::Repair { force_files: true },
        Some(&target),
        Some(&changed),
        Some(&ledger),
        &matches,
    )
    .unwrap();
    assert_eq!(forced.files[0].kind, FileOperationKind::RepairOwned);
}
