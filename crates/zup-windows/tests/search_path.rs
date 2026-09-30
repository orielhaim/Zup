//! Windows search-path semantics are decided by the adapter, not by the
//! portable delta planner.
//!
//! The planner only ever sees a `SearchPath` of target-normalized entries and
//! asks whether one is a member. Everything host-specific - the `;` separator,
//! quoting, case identity, `%VAR%` references, `REG_SZ` vs `REG_EXPAND_SZ` -
//! belongs here.

use std::collections::BTreeMap;

use zup_core::{Privilege, ResourceKey, SelectedScope, TargetTriple};
use zup_exec::{
    HostSnapshot, InstallLedger, LifecycleAction, ObservedPathEntry, PathOperationKind,
    plan_execution, plan_lifecycle,
};
use zup_platform::TargetPath;
use zup_windows::VALUE_TYPE_EXPAND;

fn windows() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn tpath(value: &str) -> TargetPath {
    TargetPath::new(windows(), value).unwrap()
}

// `split_search_path`, `search_path_contains` and `write_value_type` are unit
// tested in `src/search_path.rs`, where they are private. What is left here is
// the decision the delta planner makes from their answers.
fn target_with_path_entry(scope: SelectedScope, privilege: Privilege) -> zup_platform::TargetPlan {
    zup_platform::TargetPlan {
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
        path_entries: vec![zup_platform::TargetPathEntry {
            key: ResourceKey::PathEntry {
                value: r"C:\PF\Acme\bin".into(),
            },
            value: tpath(r"C:\PF\Acme\bin"),
            scope,
            privilege,
        }],
        services: vec![],
        protocols: vec![],
        file_associations: vec![],
        summary: zup_platform::TargetPlanSummary {
            file_count: 0,
            install_bytes: 0,
            resource_count: 1,
            requires_authorization: privilege == Privilege::System,
            selected_component_count: 0,
            prerequisite_count: 0,
            download_bytes: 0,
        },
        ui: None,
    }
}

fn snapshot_for(target: &zup_platform::TargetPlan, stored: &[&str]) -> HostSnapshot {
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

fn ledger_owning(target: &zup_platform::TargetPlan, privilege: Privilege) -> InstallLedger {
    let mut ledger = InstallLedger::new(target.app.id.clone(), target.target.clone(), target.scope);
    ledger.version = "1.0.0".parse().unwrap();
    ledger.resources.insert(
        target.path_entries[0].key.clone(),
        zup_exec::OwnedResource::PathEntry {
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
    // Unrelated values, a differently-cased duplicate, and the entry itself.
    let stored = [r"C:\Windows", r"c:/pf/acme/bin/", r"D:\tools"];
    let plan = plan_execution(&target, &snapshot_for(&target, &stored), Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);
    assert!(plan.path_entries[0].present);
    assert!(plan.path_entries[0].previously_owned);
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
    assert!(plan.path_entries[0].previously_owned);
    assert!(matches!(
        plan.path_entries[0].conflict,
        Some(zup_exec::Conflict::PathEntryConflict { .. })
    ));
    assert_eq!(plan.summary.path_entries_conflict, 1);
    assert_eq!(plan.summary.path_entries_add, 0);
}

#[test]
fn repair_restores_a_removed_owned_entry() {
    let target = target_with_path_entry(SelectedScope::Machine, Privilege::System);
    let ledger = ledger_owning(&target, Privilege::System);
    let owned_matches = BTreeMap::from([(target.path_entries[0].key.clone(), true)]);
    let plan = plan_lifecycle(
        LifecycleAction::Repair { force_files: false },
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
    assert!(plan.uninstall);
    let removal = &plan.removals[0];
    // A per-user install that owned a host-wide entry removes it as such; the
    // scope would have said User and been wrong.
    assert_eq!(removal.scope, SelectedScope::User);
    assert_eq!(removal.privilege, Privilege::System);
    assert!(plan.summary.requires_authorization);
}
