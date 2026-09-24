//! Desired-state planner tests.

use std::collections::BTreeSet;
use std::fs;

use rstest::rstest;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{Sha256Digest, materialize};
use zup_core::{ComponentId, InstallScope, Privilege, ProtocolScheme, ServiceId, Template};
use zup_manifest::{parse, parse_and_compile};
use zup_plan::{
    ComponentOverrides, InstallPlan, PlanError, PlanRequest, ResourceKey, SelectedScope, plan,
};

fn build_plan_from(source: &str, files: &[(&str, &[u8])]) -> zup_build::BuildPlan {
    let dir = TempDir::new().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let manifest = parse(source).expect("parse");
    let installer = parse_and_compile(source).expect("compile");
    materialize(&dir.path().join("zup.toml"), &manifest, installer).expect("materialize")
}

const BASE: &str = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.4.0"

[source]
directory = "dist"

[install]
scope = "either"

[install.directory]
user = "${known.local_app_data}/Programs/${app.name}"
machine = "${known.program_files}/${app.name}"
"#;

fn with(body: &str) -> String {
    format!("{BASE}\n{}", body.trim_start())
}

fn ids(values: &[&str]) -> BTreeSet<ComponentId> {
    values
        .iter()
        .map(|v| ComponentId::new(*v).unwrap())
        .collect()
}

fn overrides(enable: &[&str], disable: &[&str]) -> ComponentOverrides {
    ComponentOverrides {
        enable: ids(enable),
        disable: ids(disable),
    }
}

fn selected_names(plan: &InstallPlan) -> Vec<String> {
    plan.selected_components
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

// --- Scope ---

#[rstest]
#[case::user_manifest_user(InstallScope::User, SelectedScope::User, true)]
#[case::user_manifest_machine(InstallScope::User, SelectedScope::Machine, false)]
#[case::machine_manifest_machine(InstallScope::Machine, SelectedScope::Machine, true)]
#[case::machine_manifest_user(InstallScope::Machine, SelectedScope::User, false)]
#[case::either_user(InstallScope::Either, SelectedScope::User, true)]
#[case::either_machine(InstallScope::Either, SelectedScope::Machine, true)]
fn scope_rules(#[case] allowed: InstallScope, #[case] requested: SelectedScope, #[case] ok: bool) {
    let allowed_str = match allowed {
        InstallScope::User => "user",
        InstallScope::Machine => "machine",
        InstallScope::Either => "either",
    };
    let dir_body = match allowed {
        InstallScope::User => "[install.directory]\nuser = \"${known.local_app_data}/Acme\"",
        InstallScope::Machine => "[install.directory]\nmachine = \"${known.program_files}/Acme\"",
        InstallScope::Either => {
            "[install.directory]\nuser = \"${known.local_app_data}/Acme\"\nmachine = \"${known.program_files}/Acme\""
        }
    };
    let source = format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[source]
directory = "dist"

[install]
scope = "{allowed_str}"

{dir_body}
"#
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(requested));
    assert_eq!(result.is_ok(), ok, "err: {:?}", result.err());
    if !ok {
        assert!(matches!(result, Err(PlanError::ScopeNotAllowed { .. })));
    }
}

#[test]
fn either_user_selects_user_directory() {
    let build = build_plan_from(&with(""), &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.scope, SelectedScope::User);
    assert_eq!(
        result.install_directory.to_string(),
        "${known.local_app_data}/Programs/Acme"
    );
}

#[test]
fn either_machine_selects_machine_directory() {
    let build = build_plan_from(&with(""), &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::Machine)).unwrap();
    assert_eq!(
        result.install_directory.to_string(),
        "${known.program_files}/Acme"
    );
}

#[test]
fn author_gated_install_directory_override_reaches_the_plan() {
    let source = BASE.replace(
        "[install.directory]",
        "allow_directory_override = true\n\n[install.directory]",
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let mut request = PlanRequest::new(SelectedScope::User);
    request.install_directory = Some(Template::parse(r"C:\Apps\Acme").unwrap());
    let result = plan(&build, &request).unwrap();
    assert_eq!(result.install_directory.to_string(), r"C:\Apps\Acme");
}

#[test]
fn install_directory_override_is_rejected_when_not_authored() {
    let build = build_plan_from(BASE, &[("dist/a.txt", b"a")]);
    let mut request = PlanRequest::new(SelectedScope::User);
    request.install_directory = Some(Template::parse(r"C:\Apps\Acme").unwrap());
    assert!(matches!(
        plan(&build, &request),
        Err(PlanError::InstallDirectoryOverrideNotAllowed)
    ));
}

// --- Components ---

const GRAPH: &str = r#"
[[components]]
id = "core"
name = "Core"
required = true

[[components]]
id = "cli"
name = "CLI"
default = true
requires = ["core"]

[[components]]
id = "developer"
name = "Developer"
default = false
requires = ["cli"]

[[components]]
id = "service"
name = "Service"
default = false
requires = ["core"]
"#;

fn graph_plan(request: PlanRequest) -> Result<InstallPlan, PlanError> {
    let build = build_plan_from(&with(GRAPH), &[("dist/a.txt", b"a")]);
    plan(&build, &request)
}

#[test]
fn defaults_select_required_and_default() {
    let result = graph_plan(PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(selected_names(&result), ["core", "cli"]);
}

#[test]
fn required_component_always_selected() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&[], &["cli"]),
    })
    .unwrap();
    assert_eq!(selected_names(&result), ["core"]);
}

#[test]
fn explicit_enable_pulls_transitive_dependencies() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&["developer"], &[]),
    })
    .unwrap();
    assert_eq!(selected_names(&result), ["core", "cli", "developer"]);
}

#[test]
fn explicit_disable_of_default() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&[], &["cli"]),
    })
    .unwrap();
    assert_eq!(selected_names(&result), ["core"]);
}

#[test]
fn disable_required_errors() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&[], &["core"]),
    });
    assert!(matches!(
        result,
        Err(PlanError::RequiredComponentDisabled { .. })
    ));
}

#[test]
fn disable_required_dependency_conflicts() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&["developer"], &["core"]),
    });
    assert!(matches!(
        result,
        Err(PlanError::RequiredComponentDisabled { .. })
    ));

    let source = with(
        r#"
[[components]]
id = "base"
name = "Base"
required = false
default = true

[[components]]
id = "extra"
name = "Extra"
default = true
requires = ["base"]
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&[], &["base"]),
        },
    );
    assert!(matches!(
        result,
        Err(PlanError::DependencyExplicitlyDisabled { .. })
    ));
}

#[test]
fn enable_and_disable_same_id_errors() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&["cli"], &["cli"]),
    });
    assert!(matches!(
        result,
        Err(PlanError::ComponentBothEnabledAndDisabled { .. })
    ));
}

#[test]
fn unknown_override_errors() {
    let result = graph_plan(PlanRequest {
        scope: SelectedScope::User,
        install_directory: None,
        components: overrides(&["nope"], &[]),
    });
    assert!(matches!(
        result,
        Err(PlanError::UnknownComponentOverride { .. })
    ));
}

// --- Conditions ---

#[test]
fn condition_filters_resources() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
requires = ["core"]

[[path]]
value = "${install}/bin"
when = 'component("cli")'

[[path]]
value = "${install}/extra"
when = '!component("cli")'
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);

    let with_cli = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&["cli"], &[]),
        },
    )
    .unwrap();
    assert_eq!(with_cli.path_entries.len(), 1);
    assert_eq!(
        with_cli.path_entries[0].value.to_string(),
        "${known.local_app_data}/Programs/Acme/bin"
    );

    let without_cli = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&[], &["cli"]),
        },
    )
    .unwrap();
    assert_eq!(without_cli.path_entries.len(), 1);
    assert_eq!(
        without_cli.path_entries[0].value.to_string(),
        "${known.local_app_data}/Programs/Acme/extra"
    );
}

#[test]
fn component_gate_and_condition_both_required() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
requires = ["core"]

[[files]]
source = "a.txt"
destination = "${install}/a.txt"
component = "cli"
when = 'component("core") && component("cli")'
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let without_cli = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&[], &["cli"]),
        },
    )
    .unwrap();
    assert!(without_cli.files.is_empty());

    let with_cli = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&["cli"], &[]),
        },
    )
    .unwrap();
    assert_eq!(with_cli.files.len(), 1);
}

#[test]
fn nested_condition_expressions() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[path]]
value = "${install}/x"
when = '!(component("core") && component("core")) || component("core")'
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.path_entries.len(), 1);
}

// --- Files ---

#[test]
fn inactive_component_files_excluded() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
default = false
requires = ["core"]

[[files]]
source = "a.txt"
destination = "${install}/a.txt"
component = "core"

[[files]]
source = "b.txt"
destination = "${install}/b.txt"
component = "cli"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a"), ("dist/b.txt", b"bb")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].source_relative.as_str(), "a.txt");
    assert_eq!(result.files[0].size, 1);
}

#[test]
fn selected_files_retain_digest_and_relative_path() {
    let source = with(
        r#"
[[files]]
source = "bin/**/*"
destination = "${install}/tools"
"#,
    );
    let build = build_plan_from(&source, &[("dist/bin/acme.exe", b"exe!")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    let file = &result.files[0];
    assert_eq!(file.source_relative.as_str(), "bin/acme.exe");
    assert_eq!(file.size, 4);

    let mut hasher = Sha256::new();
    hasher.update(b"exe!");
    assert_eq!(file.sha256, Sha256Digest::from_hasher(hasher));
    assert_eq!(
        file.destination.to_string(),
        "${known.local_app_data}/Programs/Acme/tools/acme.exe"
    );

    // Portable plan must not serialize build-machine source paths.
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("\"source\":"));
}

// --- Collisions ---

#[test]
fn duplicate_active_shortcuts() {
    let source = with(
        r#"
[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/a.exe"

[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/b.exe"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User));
    assert!(matches!(
        result,
        Err(PlanError::ActiveShortcutCollision { .. })
    ));
}

#[test]
fn same_shortcuts_in_inactive_components_do_not_collide() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
default = false
requires = ["core"]

[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/a.exe"
component = "core"

[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/b.exe"
component = "cli"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.shortcuts.len(), 1);
}

#[test]
fn duplicate_active_protocols() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[protocols]]
scheme = "acme"
executable = "${install}/a.exe"
when = 'component("core") || component("core")'

[[protocols]]
scheme = "acme"
executable = "${install}/b.exe"
when = 'component("core")'
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User));
    assert!(matches!(
        result,
        Err(PlanError::ActiveProtocolCollision { .. })
    ));
}

#[test]
fn duplicate_extension_is_compile_error() {
    let source = with(
        r#"
[[file_types]]
extension = ".acme"
id = "Acme.A"

[[file_types]]
extension = ".acme"
id = "Acme.B"
"#,
    );
    assert!(parse_and_compile(&source).is_err());
}

#[test]
fn duplicate_path_entry() {
    let source = with(
        r#"
[[path]]
value = "${install}/bin"

[[path]]
value = "${install}/bin"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User));
    assert!(matches!(result, Err(PlanError::ActivePathCollision { .. })));
}

#[test]
fn no_false_collision_for_distinct_resources() {
    let source = with(
        r#"
[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/a.exe"

[[shortcuts]]
location = "desktop"
name = "Acme"
target = "${install}/a.exe"

[[path]]
value = "${install}/bin"

[[path]]
value = "${install}/tools"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.shortcuts.len(), 2);
    assert_eq!(result.path_entries.len(), 2);
}

// --- Privilege ---

#[test]
fn user_plan_does_not_require_elevation() {
    let source = with(
        r#"
[[files]]
source = "a.txt"
destination = "${install}/a.txt"

[[shortcuts]]
location = "desktop"
name = "Acme"
target = "${install}/a.txt"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert!(!result.summary.requires_elevation);
    assert!(result.files.iter().all(|f| f.privilege == Privilege::User));
}

#[test]
fn machine_plan_requires_elevation() {
    let source = with(
        r#"
[[files]]
source = "a.txt"
destination = "${install}/a.txt"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::Machine)).unwrap();
    assert!(result.summary.requires_elevation);
    assert!(
        result
            .files
            .iter()
            .all(|f| f.privilege == Privilege::Machine)
    );
}

#[test]
fn service_makes_plan_machine_privileged() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "service"
name = "Service"
default = false
requires = ["core"]

[[services]]
id = "acme-agent"
name = "acme-agent"
binary = "${install}/acme-agent.exe"
start = "automatic"
component = "service"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let without = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert!(!without.summary.requires_elevation);

    let with_svc = plan(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&["service"], &[]),
        },
    )
    .unwrap();
    assert!(with_svc.summary.requires_elevation);
    assert_eq!(with_svc.services[0].privilege, Privilege::Machine);
}

// --- Summary ---

#[test]
fn summary_counts_filtered_files_and_bytes() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
default = false
requires = ["core"]

[[files]]
source = "a.txt"
destination = "${install}/a.txt"
component = "core"

[[files]]
source = "b.txt"
destination = "${install}/b.txt"
component = "cli"
"#,
    );
    let build = build_plan_from(
        &source,
        &[("dist/a.txt", b"aaa"), ("dist/b.txt", b"bbbbbb")],
    );
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(result.summary.file_count, 1);
    assert_eq!(result.summary.install_bytes, 3);
    assert_eq!(result.summary.selected_component_count, 1);
    assert_eq!(result.summary.resource_count, 0);
}

// --- Determinism / serialization ---

#[test]
fn repeated_plans_are_identical() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[files]]
source = "a.txt"
destination = "${install}/a.txt"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let request = PlanRequest::new(SelectedScope::Machine);
    let a = plan(&build, &request).unwrap();
    let b = plan(&build, &request).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

#[test]
fn serde_json_roundtrip() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
requires = ["core"]

[[files]]
source = "a.txt"
destination = "${install}/bin/a.txt"
component = "core"

[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/bin/a.txt"
component = "core"

[[path]]
value = "${install}/bin"
component = "cli"

[[protocols]]
scheme = "acme"
executable = "${install}/bin/a.txt"
args = ["--url", "%1"]

[[file_types]]
extension = ".acme"
id = "Acme.Document"
executable = "${install}/bin/a.txt"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let original = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    let json = serde_json::to_string_pretty(&original).unwrap();
    let restored: InstallPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(original, restored);
}

#[test]
fn no_install_or_app_variables_remain() {
    let source = with(
        r#"
[[files]]
source = "a.txt"
destination = "${install}/${app.name}/a.txt"

[[path]]
value = "${install}/bin/${app.version}"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("${install}"));
    assert!(!json.contains("${app."));
    assert!(json.contains("${known."));
}

#[test]
fn resource_keys_are_stable() {
    let source = with(
        r#"
[[files]]
source = "a.txt"
destination = "${install}/a.txt"

[[services]]
id = "svc"
name = "svc"
binary = "${install}/svc.exe"
start = "manual"

[[protocols]]
scheme = "acme"
executable = "${install}/a.exe"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let a = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    let b = plan(&build, &PlanRequest::new(SelectedScope::User)).unwrap();
    assert_eq!(a.files[0].key, b.files[0].key);
    assert_eq!(
        a.services[0].key,
        ResourceKey::Service {
            id: ServiceId::new("svc").unwrap()
        }
    );
    assert_eq!(
        a.protocols[0].key,
        ResourceKey::Protocol {
            scheme: ProtocolScheme::new("acme").unwrap()
        }
    );
}

#[test]
fn recursive_install_directory_rejected() {
    let source = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[source]
directory = "dist"

[install]
scope = "user"

[install.directory]
user = "${install}/Acme"
"#;
    let err = parse_and_compile(source).expect_err("must fail");
    assert!(err.to_string().contains("install"));
}
