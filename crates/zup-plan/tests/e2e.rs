//! End-to-end: parse → compile → materialize → plan for the Acme fixture.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use zup_build::materialize;
use zup_core::{ComponentId, Privilege, TargetTriple, Variable};
use zup_manifest::{TargetOverrides, compile, parse, select_targets};
use zup_plan::{ComponentOverrides, PlanRequest, SelectedScope, plan};

fn write(path: PathBuf, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn acme_project() -> (TempDir, zup_build::BuildPlan) {
    let dir = TempDir::new().unwrap();
    write(dir.path().join("zup.toml"), b"");
    write(dir.path().join("dist/Acme.exe"), b"main-exe");
    write(dir.path().join("dist/acme-agent.exe"), b"agent-exe");
    write(dir.path().join("dist/bin/acme.exe"), b"cli-exe");
    write(dir.path().join("plugins/setup-helper.wasm"), b"plugin");

    let source = include_str!("fixtures/acme.toml");
    let manifest = parse(source).expect("parse acme");
    let config = select_targets(&manifest, &["windows-x64"], &TargetOverrides::default())
        .expect("target")
        .into_iter()
        .next()
        .expect("selected target");
    let installer = compile(&manifest, &config, &TargetOverrides::default()).expect("compile acme");
    let build = materialize(
        &dir.path().join("zup.toml"),
        &manifest,
        vec![(config, installer)],
    )
    .expect("materialize");
    (dir, build)
}

fn ids(values: &[&str]) -> BTreeSet<ComponentId> {
    values
        .iter()
        .map(|v| ComponentId::new(*v).unwrap())
        .collect()
}

#[test]
fn user_default_plan() {
    let (_dir, build) = acme_project();
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).expect("user plan");

    let selected: Vec<_> = result
        .selected_components
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    // core required + cli default; service default=false
    assert_eq!(selected, ["core", "cli"]);

    // Service component not selected → no service resource.
    assert!(result.services.is_empty());

    // User install directory with app.name resolved.
    assert_eq!(
        result.install_directory.to_string(),
        "${location.user_data}/Programs/Acme"
    );

    // No ${install} / ${app.*} remain.
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("${install}"));
    assert!(!json.contains("${app."));
    assert!(json.contains("${location."));

    // core files: payload under component core + launcher/path/cli files.
    // Fixture: files **/* component=core; path component=cli; launcher core.
    assert!(!result.files.is_empty());
    assert!(!result.launchers.is_empty());
    assert_eq!(result.path_entries.len(), 1);

    assert!(!result.summary.requires_authorization);
    assert_eq!(result.summary.selected_component_count, 2);

    // Correct byte count = sum of active file sizes only.
    let expected_bytes: u64 = result.files.iter().map(|f| f.size).sum();
    assert_eq!(result.summary.install_bytes, expected_bytes);
    assert!(expected_bytes > 0);
}

#[test]
fn machine_plan_with_service_enabled() {
    let (_dir, build) = acme_project();
    let request = PlanRequest {
        target: target(),
        scope: SelectedScope::Machine,
        install_directory: None,
        components: ComponentOverrides {
            enable: ids(&["service"]),
            disable: BTreeSet::new(),
        },
    };
    let result = plan(&build, &request).expect("machine plan");

    let selected: Vec<_> = result
        .selected_components
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(selected, ["core", "cli", "service"]);

    assert_eq!(result.services.len(), 1);
    assert_eq!(result.services[0].id.as_str(), "acme-agent");
    assert_eq!(result.services[0].privilege, Privilege::System);

    assert_eq!(
        result.install_directory.to_string(),
        "${location.programs}/Acme"
    );

    assert!(result.summary.requires_authorization);

    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("${install}"));
    assert!(!json.contains("C:\\"));
    assert!(!json.contains("/dist/"));
}

#[test]
fn install_directory_variables_resolved_into_destinations() {
    let (_dir, build) = acme_project();
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::Machine)).unwrap();

    // file destination `${install}` + relative path expands install directory.
    for file in &result.files {
        let dest = file.destination.to_string();
        assert!(
            dest.starts_with("${location.programs}/Acme"),
            "dest: {dest}"
        );
    }
    assert!(
        !result
            .install_directory
            .contains_variable(Variable::Install)
    );
}
