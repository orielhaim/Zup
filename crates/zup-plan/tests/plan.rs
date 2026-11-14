use std::collections::BTreeSet;
use std::fs;

use rstest::rstest;
use tempfile::TempDir;
use zup_build::materialize;
use zup_core::TargetOverrides;
use zup_core::{ComponentId, InstallScope, Privilege, ProtocolScheme, ServiceId, Target, Template};
use zup_core::{ResourceKey, SelectedScope};
use zup_manifest::{compile, parse, parse_and_compile, select_targets};
use zup_plan::{ComponentOverrides, DesiredState, PlanError, PlanRequest, plan};

fn target() -> Target {
    Target::parse("windows-x64").unwrap()
}

fn build_plan_from(source: &str, files: &[(&str, &[u8])]) -> zup_core::BuildPlan {
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
target = "windows-x64"
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

fn selected_names(plan: &DesiredState) -> Vec<String> {
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
    let result = plan(
        &build,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert_eq!(result.prerequisites.len(), 1);
    assert_eq!(result.prerequisites[0].id.as_str(), "vc-runtime");
    assert_eq!(result.summary().unwrap().prerequisite_count, 1);
    assert_eq!(result.summary().unwrap().download_bytes, 1024);
    assert!(result.summary().unwrap().requires_authorization);
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
    let result = plan(
        &build,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert!(result.prerequisites.is_empty());
}

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
target = "windows-x64"
source = {{ directory = "dist" }}

[install]
scope = "{allowed_str}"

{dir_body}
"#
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let result = plan(
        &build,
        &PlanRequest::new(target(), requested),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan);
    assert_eq!(result.is_ok(), ok, "err: {:?}", result.err());
    if !ok {
        assert!(matches!(result, Err(PlanError::ScopeNotAllowed { .. })));
    }
}

#[test]
fn install_directory_override_is_rejected_when_not_authored() {
    let build = build_plan_from(BASE, &[("dist/a.txt", b"a")]);
    let mut request = PlanRequest::new(target(), SelectedScope::User);
    request.install_directory = Some(zup_plan::InstallDirectory::Override(
        Template::parse(r"C:\Apps\Acme").unwrap(),
    ));
    assert!(matches!(
        plan(&build, &request, None, &zup_plan::NeverCancelled)
            .map(|installation| installation.plan),
        Err(PlanError::InstallDirectoryOverrideNotAllowed)
    ));
}

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

fn graph_plan(request: PlanRequest) -> Result<DesiredState, PlanError> {
    let build = build_plan_from(&with(GRAPH), &[("dist/a.txt", b"a")]);
    plan(&build, &request, None, &zup_plan::NeverCancelled).map(|installation| installation.plan)
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
    let result = graph_plan(PlanRequest {
        components: overrides(&["developer"], &["core"]),
        ..PlanRequest::new(target(), SelectedScope::User)
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
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan);
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
id = "cli-1"
value = "${install}/bin"
when = 'component("cli")'

[[path]]
id = "cli-2"
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
            ..PlanRequest::new(target(), SelectedScope::User)
        },
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
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
            ..PlanRequest::new(target(), SelectedScope::User)
        },
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
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
id = "payload-1"
source = "a.txt"
placement = { kind = "fixed_file", path = "${install}/a.txt" }
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
            ..PlanRequest::new(target(), SelectedScope::User)
        },
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert!(without_cli.files.is_empty());

    let with_cli = plan(
        &build,
        &PlanRequest {
            target: target(),
            scope: SelectedScope::User,
            install_directory: None,
            components: overrides(&["cli"], &[]),
            ..PlanRequest::new(target(), SelectedScope::User)
        },
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert_eq!(with_cli.files.len(), 1);
}

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
id = "payload-2"
source = "a.txt"
placement = { kind = "fixed_file", path = "${install}/a.txt" }
component = "core"

[[files]]
id = "payload-3"
source = "b.txt"
placement = { kind = "fixed_file", path = "${install}/b.txt" }
component = "cli"
"#,
    );
    let build = build_plan_from(
        &source,
        &[("dist/a.txt", b"aaa"), ("dist/b.txt", b"bbbbbb")],
    );
    let result = plan(
        &build,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].source_relative.as_str(), "a.txt");
    assert_eq!(result.files[0].size, 3);
    assert_eq!(result.summary().unwrap().file_count, 1);
    assert_eq!(result.summary().unwrap().install_bytes, 3);
    assert_eq!(result.summary().unwrap().selected_component_count, 1);
    assert_eq!(result.summary().unwrap().resource_count, 0);
}

#[test]
fn selected_files_retain_digest_and_relative_path() {
    let source = with(
        r#"
[[files]]
id = "payload-4"
source = "bin/**/*"
placement = { kind = "fixed", directory = "${install}/tools" }
"#,
    );
    let build = build_plan_from(&source, &[("dist/bin/acme.exe", b"exe!")]);
    let result = plan(
        &build,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    let file = &result.files[0];
    assert_eq!(file.source_relative.as_str(), "bin/acme.exe");
    assert_eq!(file.size, 4);

    assert_eq!(file.sha256, zup_core::hash_bytes(b"exe!"));
    assert_eq!(
        file.destination().to_string(),
        "${location.user_data}/Programs/Acme/tools/acme.exe"
    );

    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("\"source\":"));
}

#[rstest]
#[case::launchers(
    r#"
[[launchers]]
id = "main"
location = "menu"
name = "Acme"
target = "${install}/a.exe"

[[launchers]]
id = "launcher-2"
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
id = "cli-3"
value = "${install}/bin"

[[path]]
id = "cli-4"
value = "${install}/bin"
"#,
    None
)]
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
id = "launcher-3"
location = "menu"
name = "Acme"
target = "${install}/a.exe"
component = "core"

[[launchers]]
id = "launcher-4"
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
id = "launcher-5"
location = "menu"
name = "Acme"
target = "${install}/a.exe"

[[launchers]]
id = "launcher-6"
location = "desktop"
name = "Acme"
target = "${install}/a.exe"

[[path]]
id = "cli-5"
value = "${install}/bin"

[[path]]
id = "cli-6"
value = "${install}/tools"
"#,
    Some(4)
)]
fn resources_collide_only_while_they_are_both_active(
    #[case] body: &str,
    #[case] survivors: Option<usize>,
) {
    let build = build_plan_from(&with(body), &[("dist/a.txt", b"a")]);
    let result = plan(
        &build,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan);
    let Some(survivors) = survivors else {
        let error = result.expect_err("expected a collision");
        let collided = matches!(error, PlanError::ResourceCollision { .. });
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
id = "payload-5"
source = "a.txt"
placement = { kind = "fixed_file", path = "${install}/a.txt" }

[[launchers]]
id = "launcher-7"
location = "desktop"
name = "Acme"
target = "${install}/a.txt"

[[path]]
id = "cli-7"
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
    let result = plan(
        &build,
        &PlanRequest::new(target(), scope),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert_eq!(result.summary().unwrap().requires_authorization, authorized);

    assert_eq!(result.files[0].privilege, privilege);
    assert_eq!(result.launchers[0].privilege, privilege);

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
fn system_services_require_machine_ownership_when_selected() {
    let source = with(
        r#"
[[components]]
id = "service"
name = "Service"
default = false

[[services]]
execution = { type = "windows_job", account = "local_service" }
id = "acme-agent"
display_name = "acme-agent"
binary = "${install}/acme-agent.exe"
start = "automatic"
component = "service"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let user = PlanRequest::new(target(), SelectedScope::User);

    let without = plan(&build, &user, None, &zup_plan::NeverCancelled)
        .map(|installation| installation.plan)
        .unwrap();
    assert!(without.services.is_empty());
    assert!(!without.summary().unwrap().requires_authorization);

    let with_svc = plan(
        &build,
        &PlanRequest {
            components: overrides(&["service"], &[]),
            ..user
        },
        None,
        &zup_plan::NeverCancelled,
    )
    .unwrap_err();
    assert!(matches!(
        with_svc,
        PlanError::Contract(zup_plan::ContractError::Capability(_))
    ));
}

#[test]
fn repeated_plans_are_identical_and_fully_resolved() {
    let source = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[files]]
id = "payload-6"
source = "a.txt"
placement = { kind = "fixed_file", path = "${install}/${app.name}/a.txt" }

[[path]]
id = "cli-8"
value = "${install}/bin/${app.version}"

[[services]]
execution = { type = "windows_job", account = "local_service" }
id = "svc"
display_name = "svc"
binary = "${install}/svc.exe"
start = "manual"

[[protocols]]
scheme = "acme"
executable = "${install}/a.exe"
"#,
    );
    let build = build_plan_from(&source, &[("dist/a.txt", b"a")]);
    let request = PlanRequest::new(target(), SelectedScope::Machine);
    let a = plan(&build, &request, None, &zup_plan::NeverCancelled)
        .map(|installation| installation.plan)
        .unwrap();
    let b = plan(&build, &request, None, &zup_plan::NeverCancelled)
        .map(|installation| installation.plan)
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );

    let json = serde_json::to_string(&a).unwrap();
    assert!(!json.contains("${install}"), "json: {json}");
    assert!(!json.contains("${app."), "json: {json}");
    assert!(json.contains("${location."), "json: {json}");

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

#[rstest]
#[case::selected(true)]
#[case::unselected(false)]
fn service_component_dependencies_expand_to_managed_files_or_refuse_selection(
    #[case] selected: bool,
) {
    let source = with(
        r#"
[[components]]
id = "configuration"
name = "Configuration"
default = false
[[files]]
id = "payload-7"
source = "worker.exe"
placement = { kind = "fixed", directory = "${install}" }
executable = true
[[files]]
id = "payload-8"
source = "worker.conf"
placement = { kind = "fixed", directory = "${install}" }
component = "configuration"
[[services]]
id = "worker"
display_name = "Worker"
binary = "${install}/worker.exe"
execution = { type = "windows_job", account = "local_service" }
start = "manual"
requires_components = ["configuration"]
"#,
    );
    let build = build_plan_from(
        &source,
        &[
            ("dist/worker.exe", b"worker"),
            ("dist/worker.conf", b"configuration"),
        ],
    );
    let mut request = PlanRequest::new(target(), SelectedScope::Machine);
    if selected {
        request.components = overrides(&["configuration"], &[]);
    }
    let result = plan(&build, &request, None, &zup_plan::NeverCancelled);
    if selected {
        let installation = result.unwrap();
        assert_eq!(installation.plan.services[0].requires_files.len(), 1);
        assert!(
            installation.plan.services[0].requires_files[0]
                .to_string()
                .ends_with("worker.conf")
        );
    } else {
        assert!(matches!(
            result,
            Err(PlanError::Contract(zup_plan::ContractError::Capability(_)))
        ));
    }
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
target = "windows-x64"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${install}/Acme"
"#;
    let err = parse_and_compile(source, "default").expect_err("must fail");
    assert!(err.to_string().contains("install"));
}

#[rstest]
#[case("windows-x64", "windows_job", "KnownWorker")]
#[case("linux-x64", "systemd_dynamic_user", "known-worker.service")]
fn service_names_are_application_scoped_with_explicit_native_overrides(
    #[case] target_name: &str,
    #[case] execution: &str,
    #[case] native_name: &str,
) {
    let account = if execution == "windows_job" {
        ", account = \"local_service\""
    } else {
        ""
    };
    let source = with(&format!(
        r#"
[[files]]
id = "worker"
source = "worker"
executable = true
[[services]]
id = "worker"
binary = "${{payload}}/worker"
arguments = ["${{service.worker}}"]
execution = {{ type = "{execution}"{account} }}
start = "manual"
"#
    ))
    .replace("windows-x64", target_name);
    let request = PlanRequest::new(Target::parse(target_name).unwrap(), SelectedScope::Machine);
    let planned = |source: &str| {
        let build = build_plan_from(source, &[("dist/worker", b"worker")]);
        plan(&build, &request, None, &zup_plan::NeverCancelled)
            .unwrap()
            .plan
    };
    let first = planned(&source);
    let second = planned(&source.replace("com.example.acme", "com.example.other"));
    assert_eq!(first.services[0].id, second.services[0].id);
    assert_ne!(first.services[0].name, second.services[0].name);
    assert_eq!(
        first.services[0].display_name.as_ref().unwrap().as_str(),
        "worker"
    );
    let explicit_source = source.replace(
        "id = \"worker\"\nbinary",
        &format!("id = \"worker\"\nnative_name = \"{native_name}\"\nbinary"),
    );
    let explicit = planned(&explicit_source);
    assert_eq!(explicit.services[0].name.as_str(), native_name);
    let collision = if target_name.starts_with("windows") {
        native_name.to_lowercase()
    } else {
        native_name.into()
    };
    let duplicate = format!(
        r#"{explicit_source}
[[services]]
id = "another"
native_name = "{collision}"
binary = "${{payload}}/worker"
execution = {{ type = "{execution}"{account} }}
start = "manual"
"#
    );
    let build = build_plan_from(&duplicate, &[("dist/worker", b"worker")]);
    assert!(plan(&build, &request, None, &zup_plan::NeverCancelled).is_err());
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
target = "linux-arm64"
source = { directory = "dist/linux-arm64" }

[build.targets.windows-x64]
target = "windows-x64"
source = { directory = "dist/windows-x64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Targets"

[[files]]
id = "payload-9"
source = "**/*"
placement = { kind = "fixed", directory = "${install}" }
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
    let target = Target::parse("windows-x64").unwrap();

    let result = plan(
        &build,
        &PlanRequest::new(target, SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap();
    assert_eq!(result.target, target);
    assert_eq!(result.files[0].size, 7);

    let error = plan(
        &build,
        &PlanRequest::new(Target::parse("windows-arm64").unwrap(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .map(|installation| installation.plan)
    .unwrap_err();
    assert!(matches!(error, PlanError::UnknownBuildTarget { .. }));
}

#[test]
fn an_explicit_group_requires_an_optional_component_in_every_frontend() {
    let mut build = build_plan_from(&with(GRAPH), &[("dist/a.txt", b"a")]);
    let installer = &mut build.targets[0].installer;
    let group = zup_core::NonEmptyString::new("tools").unwrap();
    installer.component_groups.push(zup_core::ComponentGroup {
        id: group.clone(),
        label: None,
        description: None,
        prominence: Default::default(),
        selection: zup_core::SelectionRequirement::Explicit,
    });
    let cli = installer
        .components
        .iter_mut()
        .find(|component| component.id.as_str() == "cli")
        .unwrap();
    cli.group = Some(group);
    cli.default = false;
    let mut request = PlanRequest::new(target(), SelectedScope::User);
    assert!(matches!(
        plan(&build, &request, None, &zup_plan::NeverCancelled)
            .map(|installation| installation.plan),
        Err(PlanError::ComponentGroupSelectionRequired { .. })
    ));
    request
        .components
        .enable
        .insert(ComponentId::new("cli").unwrap());
    assert!(
        plan(&build, &request, None, &zup_plan::NeverCancelled)
            .map(|installation| installation.plan)
            .is_ok()
    );
}

#[test]
fn deployment_contract_excludes_materialization_but_covers_payload_and_launch_behavior() {
    let source = with(
        r#"
[[files]]
id = "application"
source = "app.exe"
[[files]]
id = "notes"
source = "app.dat"
[[launchers]]
id = "launcher-8"
location = "menu"
name = "Acme"
target = "${payload}/app.exe"
"#,
    );
    let build = build_plan_from(
        &source,
        &[("dist/app.exe", b"build one"), ("dist/app.dat", b"data")],
    );
    let selected = &build;
    let prepare = || {
        plan(
            selected,
            &PlanRequest::new(target(), SelectedScope::User),
            None,
            &zup_plan::NeverCancelled,
        )
        .unwrap()
        .plan
    };
    let a = prepare();
    let b = prepare();
    assert_ne!(
        a.generation.as_ref().unwrap().id,
        b.generation.as_ref().unwrap().id
    );
    let contract = zup_plan::DeploymentRequirements::capture(&a).unwrap();
    assert_eq!(
        contract,
        zup_plan::DeploymentRequirements::capture(&b).unwrap()
    );
    assert_eq!(a.launchers[0].target.to_string(), "${payload}/app.exe");
    assert!(matches!(&contract.files[0].placement,
        zup_core::PayloadPlacement::Generation { path } if path.as_str() == "app.exe"));
    let mut reordered = contract.clone();
    reordered.files.reverse();
    assert_eq!(contract.identity(), reordered.identity());
    assert!(reordered.validate().is_err());
    let mut changed_launch = b.clone();
    changed_launch.launchers[0]
        .arguments
        .push("--different-mode".into());
    assert_ne!(
        contract.identity(),
        zup_plan::DeploymentRequirements::capture(&changed_launch)
            .unwrap()
            .identity()
    );
    let different = build_plan_from(
        &source,
        &[("dist/app.exe", b"build two"), ("dist/app.dat", b"data")],
    );
    let c = plan(
        &different,
        &PlanRequest::new(target(), SelectedScope::User),
        None,
        &zup_plan::NeverCancelled,
    )
    .unwrap();
    assert_eq!(a.app.version, c.plan.app.version);
    assert_ne!(
        contract.identity(),
        zup_plan::DeploymentRequirements::capture(&c.plan)
            .unwrap()
            .identity()
    );
}
