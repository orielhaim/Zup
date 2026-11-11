use std::collections::BTreeMap;
use zup_core::{ActivationRevision, SelectedScope};
use zup_plan::{
    DeploymentSnapshot, HostSnapshot, InstallationRecord, InstallationState, IntegrationChangeKind,
    LauncherState, LifecycleAction, ObservedLauncher, ObservedLauncherState, ObservedPathEntry,
    ObservedProtocol, ObservedProtocolState, OwnedResource, PathOperationKind, PlanRequest,
    ProtocolState,
};

fn snapshot(deployment: &zup_plan::BoundDeployment) -> HostSnapshot {
    HostSnapshot {
        launchers: deployment
            .launchers
            .iter()
            .map(|launcher| ObservedLauncher {
                key: launcher.key.clone(),
                launcher_path: launcher.launcher_path.clone(),
                state: ObservedLauncherState::Launcher {
                    target: launcher.target.clone(),
                    arguments: launcher.arguments.clone(),
                    working_directory: launcher.working_directory.clone(),
                },
            })
            .collect(),
        path_entries: deployment
            .path_entries
            .iter()
            .map(|entry| ObservedPathEntry {
                key: entry.key.clone(),
                desired: entry.value.clone(),
                scope: entry.scope,
                search_path: vec![entry.value.clone()].into_iter().collect(),
            })
            .collect(),
        protocols: deployment
            .protocols
            .iter()
            .map(|protocol| ObservedProtocol {
                key: protocol.key.clone(),
                scheme: protocol.scheme.clone(),
                scope: protocol.scope,
                state: ObservedProtocolState::Registration {
                    command: protocol.command.clone(),
                    url_protocol_marker: true,
                },
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn repair_rebinds_only_exact_declared_generation_references() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("payload")).unwrap();
    std::fs::write(root.path().join("payload/app.exe"), b"application").unwrap();
    let manifest = zup_manifest::parse(
        r#"
schema = 1
[app]
id = "example.repair-binding"
name = "Application"
version = "1.0.0"
[build]
[build.targets.default]
target = "windows-x64"
source = { directory = "payload" }
[install]
scope = "user"
[install.directory]
user = "C:/Application"
[[files]]
id = "application"
source = "app.exe"
[[launchers]]
id = "main"
location = "menu"
name = "Application"
target = "${payload}/app.exe"
working_directory = "${payload}"
[[path]]
id = "cli"
value = "${payload}"
[[protocols]]
scheme = "app-test"
executable = "${payload}/app.exe"
"#,
    )
    .unwrap();
    let overrides = zup_core::TargetOverrides::default();
    let selected: Vec<_> = zup_manifest::select_targets(&manifest, &["default"], &overrides)
        .unwrap()
        .into_iter()
        .map(|config| {
            let installer = zup_manifest::compile(&manifest, &config, &overrides).unwrap();
            (config, installer)
        })
        .collect();
    let build = zup_build::materialize(
        &root.path().join("zup.toml"),
        &manifest,
        selected,
        zup_build::Writes::None,
    )
    .unwrap();
    let context = zup_windows::WindowsTargetContext::for_build(SelectedScope::User);
    let request = PlanRequest::new(zup_core::Target::WINDOWS_X64, SelectedScope::User);
    let prepare = |request: &PlanRequest| {
        let plan = zup_plan::plan(&build, request, None, &zup_plan::NeverCancelled).unwrap();
        zup_windows::resolve_target(&plan.plan, &context).unwrap()
    };
    let original = prepare(&request);
    let materialization = original
        .generation
        .as_ref()
        .unwrap()
        .bind(zup_core::Privilege::User, Default::default())
        .unwrap()
        .materialization;
    let mut installed = {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: zup_core::TransactionId::new_v7(),
            plan_hash: zup_core::hash_bytes(b"original"),
        };
        InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: original.installation,
            app_id: original.app.id.clone(),
            target: original.target,
            scope: original.scope,
            directory: original.install_directory.clone(),
            state: InstallationState::Installed,
            current: DeploymentSnapshot {
                maintenance: None,
                requirements: original.requirements.clone(),
                revision: ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: original.app.version.clone(),
                selected_components: vec![],
                release: None,
                preset: None,
                materialization: Some(materialization),
                resources: BTreeMap::new(),
            },
            retained: vec![],
        }
    };
    let launcher = &original.launchers[0];
    installed.current.resources.insert(
        launcher.key.clone(),
        OwnedResource::Launcher {
            launcher_path: launcher.launcher_path.clone(),
            privilege: launcher.privilege,
            previous: LauncherState::Absent,
            installed: LauncherState::Launcher {
                target: launcher.target.clone(),
                arguments: launcher.arguments.clone(),
                working_directory: launcher.working_directory.clone(),
            },
        },
    );
    let entry = &original.path_entries[0];
    installed.current.resources.insert(
        entry.key.clone(),
        OwnedResource::PathEntry {
            value: entry.value.clone(),
            value_type: "expand_sz".into(),
            privilege: entry.privilege,
        },
    );
    let protocol = &original.protocols[0];
    installed.current.resources.insert(
        protocol.key.clone(),
        OwnedResource::Protocol {
            previous: ProtocolState::Absent,
            installed: ProtocolState::Registration {
                command: protocol.command.clone(),
            },
            privilege: protocol.privilege,
        },
    );
    let mut repair = request;
    repair.materialization = zup_core::MaterializationId::new_v7();
    repair.previous = Some(installed.current.selection());
    repair.generation_policy = zup_plan::GenerationPolicy::Reconstruct;
    let desired = prepare(&repair);
    let mut observed = snapshot(&original);
    observed.path_entries[0].desired = desired.path_entries[0].value.clone();
    let plan = |desired: &zup_plan::BoundDeployment,
                installed: &InstallationRecord,
                observed: &HostSnapshot| {
        zup_plan::plan_lifecycle(
            LifecycleAction::Repair { force: false },
            Some(desired),
            Some(observed),
            Some(installed),
            &BTreeMap::new(),
        )
    };
    let execution = plan(&desired, &installed, &observed).unwrap();
    assert_eq!(
        execution.launchers[0].kind,
        IntegrationChangeKind::UpdateOwned
    );
    assert_eq!(
        execution.path_entries[0].kind,
        PathOperationKind::RebindOwned
    );
    assert_eq!(
        execution.protocols[0].kind,
        IntegrationChangeKind::UpdateOwned
    );

    let mut substituted = desired.clone();
    substituted.launchers[0].target = substituted
        .generation
        .as_ref()
        .unwrap()
        .directory
        .join("other.exe")
        .unwrap();
    assert!(matches!(
        plan(&substituted, &installed, &observed),
        Err(zup_plan::LifecycleError::RepairDesiredChanged { .. })
    ));
    let mut changed = desired.clone();
    changed.launchers[0].arguments.push("--new-mode".into());
    changed.requirements.launchers[0]
        .arguments
        .push("--new-mode".into());
    assert!(matches!(
        plan(&changed, &installed, &observed),
        Err(zup_plan::LifecycleError::RepairRequirementsChanged)
    ));
}
