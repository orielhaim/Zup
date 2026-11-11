#![cfg(windows)]

use std::collections::BTreeMap;
use zup_core::TargetPath;

use zup_core::{Privilege, ResourceKey, SelectedScope, Target};
use zup_plan::{
    HostSnapshot, InstallationRecord, LifecycleAction, ObservedPathEntry, PathOperationKind,
    plan_execution, plan_lifecycle,
};

use zup_windows::VALUE_TYPE_EXPAND;

fn windows() -> Target {
    Target::parse("windows-x64").unwrap()
}

fn tpath(value: &str) -> TargetPath {
    TargetPath::new(windows(), value).unwrap()
}

fn target_with_path_entry(scope: SelectedScope, privilege: Privilege) -> zup_plan::BoundDeployment {
    zup_plan::DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: zup_core::InstallationId::new_v7(),
        generation: None,
        app: zup_core::App {
            id: zup_core::AppId::new("com.acme.acme").unwrap(),
            name: zup_core::NonEmptyString::new("Acme").unwrap(),
            version: "1.0.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: windows(),
        scope,
        install_directory: tpath(r"C:\PF\Acme"),
        selected_components: vec![],
        prerequisites: vec![],
        files: vec![],
        launchers: vec![],
        path_entries: vec![zup_plan::DesiredPathEntry {
            key: ResourceKey::PathEntry {
                id: zup_core::DeclaredResourceId::Application {
                    id: zup_core::ResourceId::new("cli").unwrap(),
                },
            },
            value: tpath(r"C:\PF\Acme\bin"),
            scope,
            privilege,
        }],
        services: vec![],
        protocols: vec![],
        file_associations: vec![],

        preset: None,
    }
    .bind_fixed()
    .expect("fixed deployment requirements")
}

fn snapshot_for(target: &zup_plan::BoundDeployment, stored: &[&str]) -> HostSnapshot {
    HostSnapshot {
        path_entries: vec![ObservedPathEntry {
            key: target.path_entries[0].key.clone(),
            desired: target.path_entries[0].value.clone(),
            scope: target.path_entries[0].scope,
            search_path: stored.iter().map(|value| tpath(value)).collect(),
        }],
        ..Default::default()
    }
}

fn ledger_owning(target: &zup_plan::BoundDeployment, privilege: Privilege) -> InstallationRecord {
    let mut ledger = {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: zup_core::TransactionId::new_v7(),
            plan_hash: zup_core::hash_bytes(b"path fixture"),
        };
        InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: target.installation,
            state: zup_plan::InstallationState::Installed,
            app_id: target.app.id.clone(),
            target: target.target,
            scope: target.scope,
            directory: target.install_directory.clone(),
            current: zup_plan::DeploymentSnapshot {
                maintenance: None,
                requirements: target.requirements.clone(),
                revision: zup_core::ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: target.app.version.clone(),
                selected_components: vec![],
                release: None,
                preset: None,
                materialization: None,
                resources: Default::default(),
            },
            retained: vec![],
        }
    };
    ledger.current.resources.insert(
        target.path_entries[0].key.clone(),
        zup_plan::OwnedResource::PathEntry {
            value: target.path_entries[0].value.clone(),
            value_type: VALUE_TYPE_EXPAND.into(),
            privilege,
        },
    );
    ledger
}

#[test]
fn ownership_ignores_unrelated_segments() {
    let target = target_with_path_entry(SelectedScope::User, Privilege::User);
    let ledger = ledger_owning(&target, Privilege::User);

    let stored = [r"C:\Windows", r"c:/pf/acme/bin/", r"D:\tools"];
    let plan = plan_execution(&target, &snapshot_for(&target, &stored), Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);
    assert!(matches!(
        plan.path_entries[0].precondition,
        zup_plan::PathPrecondition::Owned {
            previous: zup_plan::PathPresence::Single,
            desired: zup_plan::PathPresence::Single,
            ..
        }
    ));
    assert_eq!(plan.path_entries[0].scope, SelectedScope::User);
    assert_eq!(plan.path_entries[0].privilege, Privilege::User);
}

#[test]
fn a_removed_owned_entry_is_drift_not_an_add() {
    let target = target_with_path_entry(SelectedScope::Machine, Privilege::System);
    let ledger = ledger_owning(&target, Privilege::System);
    let plan = plan_execution(
        &target,
        &snapshot_for(&target, &[r"C:\Windows"]),
        Some(&ledger),
    )
    .unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Drift);
    assert!(matches!(
        plan.path_entries[0].precondition,
        zup_plan::PathPrecondition::Owned {
            previous: zup_plan::PathPresence::Missing,
            ..
        }
    ));
    assert!(matches!(
        plan.path_entries[0].conflict,
        Some(zup_plan::Conflict::PathEntryConflict { .. })
    ));
}

#[test]
fn repair_restores_a_removed_owned_entry() {
    let target = target_with_path_entry(SelectedScope::Machine, Privilege::System);
    let ledger = ledger_owning(&target, Privilege::System);
    let owned_matches = BTreeMap::from([(target.path_entries[0].key.clone(), true)]);
    let plan = plan_lifecycle(
        LifecycleAction::Repair { force: false },
        Some(&target),
        Some(&snapshot_for(&target, &[])),
        Some(&ledger),
        &owned_matches,
    )
    .unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::RestoreOwned);
    assert!(plan.path_entries[0].conflict.is_none());
    assert_eq!(plan.path_entries[0].privilege, Privilege::System);
}

#[test]
fn uninstall_removes_a_search_path_entry_with_its_recorded_authority() {
    let target = target_with_path_entry(SelectedScope::User, Privilege::User);
    let ledger = ledger_owning(&target, Privilege::System);
    let owned_matches = BTreeMap::from([(target.path_entries[0].key.clone(), true)]);
    let plan = plan_lifecycle(
        LifecycleAction::Uninstall,
        None,
        None,
        Some(&ledger),
        &owned_matches,
    )
    .unwrap();
    assert!(plan.transition.is_remove());
    let removal = &plan.removals[0];

    assert_eq!(removal.scope, SelectedScope::User);
    assert_eq!(removal.privilege, Privilege::System);
}

#[rstest::rstest]
#[case(vec![r"C:\PF\Acme\bin"], PathOperationKind::RebindOwned)]
#[case(vec![], PathOperationKind::Drift)]
#[case(vec![r"C:\PF\Acme\bin", r"c:/pf/acme/bin/"], PathOperationKind::Drift)]
#[case(vec![r"C:\PF\Acme\bin", r"C:\PF\Acme\next"], PathOperationKind::Drift)]
fn generation_switch_keeps_logical_path_ownership(
    #[case] stored: Vec<&str>,
    #[case] expected: PathOperationKind,
) {
    let mut target = target_with_path_entry(SelectedScope::User, Privilege::User);
    let ledger = ledger_owning(&target, Privilege::User);
    target.path_entries[0].value = tpath(r"C:\PF\Acme\next");
    let plan = plan_execution(&target, &snapshot_for(&target, &stored), Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, expected);
    assert_eq!(plan.path_entries[0].key, target.path_entries[0].key);
    assert!(matches!(&plan.path_entries[0].precondition,
        zup_plan::PathPrecondition::Owned { binding, .. }
        if binding.value == tpath(r"C:\PF\Acme\bin")));
}
