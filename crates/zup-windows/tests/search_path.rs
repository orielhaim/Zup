//! Windows search-path semantics are decided by the adapter, not by the
//! portable delta planner.
//!
//! The planner only ever sees a `SearchPath` of target-normalized entries and
//! asks whether one is a member. Everything host-specific — the `;` separator,
//! quoting, case identity, `%VAR%` references, `REG_SZ` vs `REG_EXPAND_SZ` —
//! belongs here.

use std::collections::BTreeMap;

use zup_core::{Privilege, ResourceKey, SelectedScope, TargetTriple};
use zup_exec::{
    HostSnapshot, InstallLedger, LifecycleAction, ObservedPathEntry, PathOperationKind,
    plan_execution, plan_lifecycle,
};
use zup_platform::TargetPath;
use zup_windows::{
    PATH_VALUE_NAME, VALUE_TYPE_EXPAND, VALUE_TYPE_MISSING, VALUE_TYPE_PLAIN, lost_expansion,
    search_path_contains, split_search_path, write_value_type,
};

fn windows() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn tpath(value: &str) -> TargetPath {
    TargetPath::new(windows(), value).unwrap()
}

#[test]
fn splitting_follows_windows_separator_rules() {
    assert_eq!(
        split_search_path(r"C:\one;C:\two;;   ;C:\three"),
        vec![r"C:\one", r"C:\two", r"C:\three"]
    );
    assert!(split_search_path("").is_empty());
    assert!(split_search_path(";;;;").is_empty());
}

#[test]
fn membership_uses_windows_path_identity() {
    let desired = tpath(r"C:\Apps\Acme\bin");
    for stored in [
        r"C:\Apps\Acme\bin",
        r"c:\apps\acme\bin",
        r"C:/Apps/Acme/bin",
        r"C:\Apps\Acme\bin\",
        r#""C:\Apps\Acme\bin""#,
        r"C:\Windows;C:\Apps\Acme\bin;D:\tools",
    ] {
        assert!(
            search_path_contains(&windows(), stored, &desired),
            "expected a match for {stored}"
        );
    }
    for stored in [
        r"C:\Apps\Acme",
        r"C:\Apps\Acme\bin2",
        r"%ProgramFiles%\Acme\bin",
        r"",
    ] {
        assert!(
            !search_path_contains(&windows(), stored, &desired),
            "expected no match for {stored}"
        );
    }
}

#[test]
fn expand_string_semantics_are_preserved() {
    assert_eq!(VALUE_TYPE_PLAIN, "sz");
    assert_eq!(VALUE_TYPE_EXPAND, "expand_sz");
    assert_eq!(VALUE_TYPE_MISSING, "missing");
    assert_eq!(PATH_VALUE_NAME, "Path");
    // A value that never existed is written as an expanding path, matching how
    // Windows creates the value itself.
    assert_eq!(write_value_type(VALUE_TYPE_MISSING), VALUE_TYPE_EXPAND);
    // An existing type is never changed, so unrelated `%VAR%` segments keep
    // expanding and a plain value is never silently promoted.
    assert_eq!(write_value_type(VALUE_TYPE_PLAIN), VALUE_TYPE_PLAIN);
    assert_eq!(write_value_type(VALUE_TYPE_EXPAND), VALUE_TYPE_EXPAND);
    // Losing a value that used to expand is drift, not a silent change.
    assert!(lost_expansion(VALUE_TYPE_MISSING, VALUE_TYPE_EXPAND));
    assert!(!lost_expansion(VALUE_TYPE_PLAIN, VALUE_TYPE_PLAIN));
    assert!(!lost_expansion(VALUE_TYPE_EXPAND, VALUE_TYPE_EXPAND));
}

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
fn ownership_decision_is_stable_across_repeated_plans() {
    let target = target_with_path_entry(SelectedScope::Machine, Privilege::System);
    let snapshot = snapshot_for(&target, &[r"C:\PF\Acme\bin"]);
    let first = plan_execution(&target, &snapshot, None).unwrap();
    let second = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.path_entries[0].kind, PathOperationKind::Present);
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
fn search_path_ownership_is_independent_of_authorization() {
    // A per-user search path entry that needs host-wide authority, and a
    // host-wide entry that needs none. Neither decision changes the other.
    let user_path = target_with_path_entry(SelectedScope::User, Privilege::System);
    let plan = plan_execution(&user_path, &snapshot_for(&user_path, &[]), None).unwrap();
    assert_eq!(plan.path_entries[0].scope, SelectedScope::User);
    assert_eq!(plan.path_entries[0].privilege, Privilege::System);
    assert!(plan.summary.requires_authorization);

    let machine_path = target_with_path_entry(SelectedScope::Machine, Privilege::User);
    let plan = plan_execution(&machine_path, &snapshot_for(&machine_path, &[]), None).unwrap();
    assert_eq!(plan.path_entries[0].scope, SelectedScope::Machine);
    assert_eq!(plan.path_entries[0].privilege, Privilege::User);
    assert!(!plan.summary.requires_authorization);
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

#[test]
fn portable_observation_names_no_host_facility() {
    let source = include_str!("../../zup-exec/src/observe.rs");
    for forbidden in ["SCM", "Registry", "registry", "HKLM", "HKCU", "UAC"] {
        assert!(
            !source.contains(forbidden),
            "portable observation names a host facility: {forbidden}"
        );
    }
}
