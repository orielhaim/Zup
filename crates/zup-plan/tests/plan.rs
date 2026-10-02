//! Desired-state planner tests.

use std::collections::BTreeSet;
use std::fs;

use rstest::rstest;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{Sha256Digest, materialize};
use zup_core::{
    ComponentId, InstallScope, Privilege, ProtocolScheme, ServiceId, TargetTriple, Template,
};
use zup_manifest::{TargetOverrides, compile, parse, parse_and_compile, select_targets};
use zup_plan::{
    ComponentOverrides, InstallPlan, PlanError, PlanRequest, ResourceKey, SelectedScope, plan,
};

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn build_plan_from(source: &str, files: &[(&str, &[u8])]) -> zup_build::BuildPlan {
    let dir = TempDir::new().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let manifest = parse(source).expect("parse");
    let selected = select_targets(&manifest, &["default"], &TargetOverrides::default())
        .expect("target")
        .into_iter()
        .map(|config| {
            let installer =
                compile(&manifest, &config, &TargetOverrides::default()).expect("compile");
            (config, installer)
        })
        .collect::<Vec<_>>();
    materialize(
        &dir.path().join("zup.toml"),
        &manifest,
        selected,
        zup_build::Writes::None,
    )
    .expect("materialize")
}

const BASE: &str = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.4.0"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "either"

[install.directory]
user = "${location.user_data}/Programs/${app.name}"
machine = "${location.programs}/${app.name}"
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

#[test]
fn active_prerequisites_follow_components_and_contribute_download_summary() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"
required = true

[[prerequisites]]
id = "vc-runtime"
name = "Visual C++ Runtime"
component = "core"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "remote", url = "https://cdn.example.test/vc.exe", filename = "vc.exe", sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", size = 1024 }

[[prerequisites]]
id = "optional-runtime"
name = "Optional Runtime"
component = "extras"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "remote", url = "https://cdn.example.test/optional.exe", filename = "optional.exe", sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", size = 2048 }

[[components]]
id = "extras"
name = "Extras"
default = false
"#,
    );
    let build = build_plan_from(&source, &[("dist/app.txt", b"app")]);
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).unwrap();
    assert_eq!(result.prerequisites.len(), 1);
    assert_eq!(result.prerequisites[0].id.as_str(), "vc-runtime");
    assert_eq!(result.summary.prerequisite_count, 1);
    assert_eq!(result.summary.download_bytes, 1024);
    assert!(result.summary.requires_authorization);
}

#[test]
fn prerequisites_are_filtered_by_condition() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"
required = true

[[prerequisites]]
id = "runtime"
name = "Runtime"
when = '!component("core")'
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "remote", url = "https://cdn.example.test/runtime.exe", filename = "runtime.exe", sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", size = 1 }
"#,
    );
    let build = build_plan_from(&source, &[("dist/app.txt", b"app")]);
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).unwrap();
    assert!(result.prerequisites.is_empty());
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
        InstallScope::User => "[install.directory]\nuser = \"${location.user_data}/Acme\"",
        InstallScope::Machine => "[install.directory]\nmachine = \"${location.programs}/Acme\"",
        InstallScope::Either => {
            "[install.directory]\nuser = \"${location.user_data}/Acme\"\nmachine = \"${location.programs}/Acme\""
        }
    };
    let source = format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = {{ directory = "dist" }}

[install]
scope = "{allowed_str}"

{dir_body}
"#
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(target(), requested));
    assert_eq!(result.is_ok(), ok, "err: {:?}", result.err());
    if !ok {
        assert!(matches!(result, Err(PlanError::ScopeNotAllowed { .. })));
    }
}

#[rstest]
#[case::user(SelectedScope::User, "${location.user_data}/Programs/Acme")]
#[case::machine(SelectedScope::Machine, "${location.programs}/Acme")]
fn either_scope_selects_its_own_directory(#[case] scope: SelectedScope, #[case] directory: &str) {
    let build = build_plan_from(&with(""), &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(target(), scope)).unwrap();
    assert_eq!(result.scope, scope);
    assert_eq!(result.install_directory.to_string(), directory);
}

#[test]
fn author_gated_install_directory_override_reaches_the_plan() {
    let source = BASE.replace(
        "[install.directory]",
        "allow_directory_override = true\n\n[install.directory]",
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let mut request = PlanRequest::new(target(), SelectedScope::User);
    request.install_directory = Some(Template::parse(r"C:\Apps\Acme").unwrap());
    let result = plan(&build, &request).unwrap();
    assert_eq!(result.install_directory.to_string(), r"C:\Apps\Acme");
}

#[test]
fn install_directory_override_is_rejected_when_not_authored() {
    let build = build_plan_from(BASE, &[("dist/a.txt", b"a")]);
    let mut request = PlanRequest::new(target(), SelectedScope::User);
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

#[rstest]
#[case::defaults(&[], &[], &["core", "cli"])]
#[case::enable_pulls_closure(&["developer"], &[], &["core", "cli", "developer"])]
#[case::disable_default(&[], &["cli"], &["core"])]
#[case::enable_a_service(&["service"], &[], &["core", "cli", "service"])]
fn component_overrides_select_the_transitive_closure(
    #[case] enable: &[&str],
    #[case] disable: &[&str],
    #[case] expected: &[&str],
) {
    let result = graph_plan(PlanRequest {
        components: overrides(enable, disable),
        ..PlanRequest::new(target(), SelectedScope::User)
    })
    .unwrap();
    assert_eq!(selected_names(&result), expected);
}

#[test]
fn disabling_a_selected_component_is_refused() {
    // A required component cannot be switched off, whether it is named
    // directly or is a dependency of a component the caller enabled.
    let result = graph_plan(PlanRequest {
        components: overrides(&["developer"], &["core"]),
        ..PlanRequest::new(target(), SelectedScope::User)
    });
    assert!(matches!(
        result,
        Err(PlanError::RequiredComponentDisabled { .. })
    ));

    // A non-required component an enabled one depends on cannot be either.
    let source = with(
        r#"
[[components]]
id = "base"
name = "Base"
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
            components: overrides(&[], &["base"]),
            ..PlanRequest::new(target(), SelectedScope::User)
        },
    );
    assert!(matches!(
        result,
        Err(PlanError::DependencyExplicitlyDisabled { .. })
    ));
}

#[test]
fn contradictory_and_unknown_component_overrides_are_refused() {
    for (enable, disable) in [(vec!["cli"], vec!["cli"]), (vec!["nope"], vec![])] {
        let result = graph_plan(PlanRequest {
            components: overrides(&enable, &disable),
            ..PlanRequest::new(target(), SelectedScope::User)
        });
        let refused = matches!(
            result,
            Err(PlanError::ComponentBothEnabledAndDisabled { .. }
                | PlanError::UnknownComponentOverride { .. })
        );
        assert!(refused, "{enable:?}/{disable:?}");
    }
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
            target: target(),
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&["cli"], &[]),
        },
    )
    .unwrap();
    assert_eq!(with_cli.path_entries.len(), 1);
    assert_eq!(
        with_cli.path_entries[0].value.to_string(),
        "${location.user_data}/Programs/Acme/bin"
    );

    let without_cli = plan(
        &build,
        &PlanRequest {
            target: target(),
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&[], &["cli"]),
        },
    )
    .unwrap();
    assert_eq!(without_cli.path_entries.len(), 1);
    assert_eq!(
        without_cli.path_entries[0].value.to_string(),
        "${location.user_data}/Programs/Acme/extra"
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
            target: target(),
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
            target: target(),
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
    // A parenthesised term nested inside `!` and `||`. Both operands are over an
    // always-selected component, so the expression is true either way; what this
    // guards is that the parser accepts the nesting at all.
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
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).unwrap();
    assert_eq!(result.path_entries.len(), 1);
}

// --- Files ---

#[test]
fn inactive_component_files_are_excluded_from_the_plan_and_its_summary() {
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
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].source_relative.as_str(), "a.txt");
    assert_eq!(result.files[0].size, 3);
    assert_eq!(result.summary.file_count, 1);
    assert_eq!(result.summary.install_bytes, 3);
    assert_eq!(result.summary.selected_component_count, 1);
    assert_eq!(result.summary.resource_count, 0);
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
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User)).unwrap();
    let file = &result.files[0];
    assert_eq!(file.source_relative.as_str(), "bin/acme.exe");
    assert_eq!(file.size, 4);

    let mut hasher = Sha256::new();
    hasher.update(b"exe!");
    assert_eq!(file.sha256, Sha256Digest::from_hasher(hasher));
    assert_eq!(
        file.destination.to_string(),
        "${location.user_data}/Programs/Acme/tools/acme.exe"
    );

    // Portable plan must not serialize build-machine source paths.
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("\"source\":"));
}

// --- Collisions ---

#[rstest]
#[case::launchers(
    r#"
[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/a.exe"

[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/b.exe"
"#,
    None
)]
#[case::protocols(
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
    None
)]
#[case::path_entries(
    r#"
[[path]]
value = "${install}/bin"

[[path]]
value = "${install}/bin"
"#,
    None
)]
// Two identical launchers, but the second only becomes active when a
// non-selected component is enabled.
#[case::gated_launchers(
    r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
default = false
requires = ["core"]

[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/a.exe"
component = "core"

[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/b.exe"
component = "cli"
"#,
    Some(1)
)]
#[case::distinct_resources(
    r#"
[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/a.exe"

[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/a.exe"

[[path]]
value = "${install}/bin"

[[path]]
value = "${install}/tools"
"#,
    Some(4)
)]
fn resources_collide_only_while_they_are_both_active(
    #[case] body: &str,
    #[case] survivors: Option<usize>,
) {
    let build = build_plan_from(&with(body), &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(target(), SelectedScope::User));
    let Some(survivors) = survivors else {
        let error = result.expect_err("expected a collision");
        let collided = matches!(
            error,
            PlanError::ActiveLauncherCollision { .. }
                | PlanError::ActiveProtocolCollision { .. }
                | PlanError::ActivePathCollision { .. }
        );
        assert!(collided, "expected a collision, got {error:?}");
        return;
    };
    let result = result.unwrap_or_else(|error| panic!("unexpected collision: {error}"));
    let total = result.launchers.len() + result.path_entries.len() + result.protocols.len();
    assert_eq!(total, survivors);
}

#[test]
fn duplicate_extension_is_compile_error() {
    let source = with(
        r#"
[[file_associations]]
extension = ".acme"
id = "Acme.A"

[[file_associations]]
extension = ".acme"
id = "Acme.B"
"#,
    );
    assert!(parse_and_compile(&source, "default").is_err());
}

// --- Privilege ---

#[rstest]
#[case::user(SelectedScope::User, Privilege::User, false)]
#[case::machine(SelectedScope::Machine, Privilege::System, true)]
fn scope_decides_the_store_and_privilege_of_registered_resources(
    #[case] scope: SelectedScope,
    #[case] privilege: Privilege,
    #[case] authorized: bool,
) {
    let source = with(
        r#"
[[files]]
source = "a.txt"
destination = "${install}/a.txt"

[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/a.txt"

[[path]]
value = "${install}/bin"

[[protocols]]
scheme = "acme"
executable = "${install}/a.txt"

[[file_associations]]
extension = ".acme"
id = "Acme.Document"
executable = "${install}/a.txt"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(&build, &PlanRequest::new(target(), scope)).unwrap();
    assert_eq!(result.summary.requires_authorization, authorized);

    assert_eq!(result.files[0].privilege, privilege);
    assert_eq!(result.launchers[0].privilege, privilege);

    // A registration resource belongs to the store the selected scope names,
    // which is independent of the privilege each resource carries.
    for store in [
        &result.path_entries[0].scope,
        &result.protocols[0].scope,
        &result.file_associations[0].scope,
    ] {
        assert_eq!(store, &scope);
    }
    for carried in [
        &result.path_entries[0].privilege,
        &result.protocols[0].privilege,
        &result.file_associations[0].privilege,
    ] {
        assert_eq!(carried, &scope.authorization());
        assert_eq!(carried, &privilege);
    }
}

#[test]
fn a_service_makes_a_user_scope_plan_system_authorized() {
    // The scope says where the application lives. The service says what
    // authority its registration needs. A per-user install that declares a
    // service must therefore report system authorization, whether the service
    // is always selected or gated behind a component.
    let source = with(
        r#"
[[components]]
id = "service"
name = "Service"
default = false

[[services]]
id = "acme-agent"
name = "acme-agent"
binary = "${install}/acme-agent.exe"
start = "automatic"
component = "service"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let user = PlanRequest::new(target(), SelectedScope::User);

    let without = plan(&build, &user).unwrap();
    assert!(without.services.is_empty());
    assert!(!without.summary.requires_authorization);

    let with_svc = plan(
        &build,
        &PlanRequest {
            components: overrides(&["service"], &[]),
            ..user
        },
    )
    .unwrap();
    assert_eq!(with_svc.scope, SelectedScope::User);
    assert_eq!(with_svc.services[0].privilege, Privilege::System);
    assert!(with_svc.summary.requires_authorization);
}

// --- Determinism / serialization ---

#[test]
fn repeated_plans_are_identical_and_fully_resolved() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[files]]
source = "a.txt"
destination = "${install}/${app.name}/a.txt"

[[path]]
value = "${install}/bin/${app.version}"

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
    let request = PlanRequest::new(target(), SelectedScope::Machine);
    let a = plan(&build, &request).unwrap();
    let b = plan(&build, &request).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );

    // Every authoring-time variable must be resolved out of the plan.
    let json = serde_json::to_string(&a).unwrap();
    assert!(!json.contains("${install}"), "json: {json}");
    assert!(!json.contains("${app."), "json: {json}");
    assert!(json.contains("${location."), "json: {json}");

    // A resource is identified by what it registers, not by its position.
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

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${install}/Acme"
"#;
    let err = parse_and_compile(source, "default").expect_err("must fail");
    assert!(err.to_string().contains("install"));
}

#[test]
fn plan_selects_the_requested_target_and_rejects_unknown_targets() {
    let source = r#"
schema = 1

[app]
id = "com.example.targets"
name = "Targets"
version = "1.0.0"

[build]

[build.targets.linux-arm64]
target = "aarch64-unknown-linux-gnu"
source = { directory = "dist/linux-arm64" }

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Targets"

[[files]]
source = "**/*"
destination = "${install}"
"#;
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("dist/linux-arm64")).unwrap();
    fs::create_dir_all(dir.path().join("dist/windows-x64")).unwrap();
    fs::write(dir.path().join("dist/linux-arm64/app"), b"linux").unwrap();
    fs::write(dir.path().join("dist/windows-x64/app"), b"windows").unwrap();
    let manifest = parse(source).unwrap();
    let selected = select_targets(&manifest, &[], &TargetOverrides::default())
        .expect("target")
        .into_iter()
        .map(|config| {
            let installer = compile(&manifest, &config, &TargetOverrides::default()).unwrap();
            (config, installer)
        })
        .collect::<Vec<_>>();
    let build = materialize(
        &dir.path().join("zup.toml"),
        &manifest,
        selected,
        zup_build::Writes::None,
    )
    .unwrap();
    let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();

    let result = plan(
        &build,
        &PlanRequest::new(target.clone(), SelectedScope::User),
    )
    .unwrap();
    assert_eq!(result.target, target);
    assert_eq!(result.files[0].size, 7);

    let error = plan(
        &build,
        &PlanRequest::new(
            TargetTriple::parse("aarch64-pc-windows-msvc").unwrap(),
            SelectedScope::User,
        ),
    )
    .unwrap_err();
    assert!(matches!(error, PlanError::UnknownBuildTarget { .. }));
}
