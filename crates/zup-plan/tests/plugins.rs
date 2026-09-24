use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{BuildPlan, materialize};
use zup_core::{ComponentId, Privilege, SelectedScope, ServiceStart, ShortcutLocation};
use zup_manifest::{parse, parse_and_compile};
use zup_plan::{
    CancellationQuery, MAX_PLUGIN_ARGUMENTS, MAX_PLUGIN_GENERATED_FILE_BYTES, MAX_PLUGIN_RESOURCES,
    MAX_PLUGIN_STRING_BYTES, NeverCancelled, PlanError, PlanRequest, PluginArchitecture,
    PluginExecutor, PluginFailure, PluginHostFacts, PluginOperatingSystem, PluginPlanningContext,
    PluginResource, PluginResourceProposal, plan, plan_with_plugins,
};

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

fn build(source: &str) -> BuildPlan {
    let dir = TempDir::new().unwrap();
    let dist = dir.path().join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(dist.join("base.txt"), b"base").unwrap();
    fs::create_dir_all(dir.path().join("plugins")).unwrap();
    fs::write(dir.path().join("plugins/one.wasm"), b"one").unwrap();
    fs::write(dir.path().join("plugins/two.wasm"), b"two").unwrap();
    let manifest = parse(source).expect("manifest parses");
    let installer = parse_and_compile(source).expect("manifest compiles");
    materialize(&dir.path().join("zup.toml"), &manifest, installer).expect("materializes")
}

fn component_id(value: &str) -> ComponentId {
    ComponentId::new(value).unwrap()
}

fn facts() -> PluginHostFacts {
    PluginHostFacts::new(PluginOperatingSystem::Windows, PluginArchitecture::X86_64)
}

fn request() -> PlanRequest {
    PlanRequest::new(SelectedScope::User)
}

#[derive(Default)]
struct FakeExecutor {
    responses: BTreeMap<String, Result<PluginResourceProposal, PluginFailure>>,
    calls: Vec<(String, PluginPlanningContext)>,
    cancellation_seen: bool,
}

impl FakeExecutor {
    fn new(responses: impl IntoIterator<Item = (String, PluginResourceProposal)>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(id, proposal)| (id, Ok(proposal)))
                .collect(),
            calls: Vec::new(),
            cancellation_seen: false,
        }
    }

    fn failing(id: &str, failure: PluginFailure) -> Self {
        Self {
            responses: BTreeMap::from([(id.to_owned(), Err(failure))]),
            calls: Vec::new(),
            cancellation_seen: false,
        }
    }
}

impl PluginExecutor for FakeExecutor {
    fn plan(
        &mut self,
        binding: &zup_core::PluginBinding,
        context: &PluginPlanningContext,
        cancellation: &dyn CancellationQuery,
    ) -> Result<PluginResourceProposal, PluginFailure> {
        self.calls.push((binding.id.to_string(), context.clone()));
        self.cancellation_seen = true;
        let _ = cancellation.is_cancelled();
        self.responses
            .remove(binding.id.as_str())
            .unwrap_or_else(|| Ok(PluginResourceProposal::default()))
    }
}

fn generated(destination: &str, bytes: &[u8]) -> PluginResource {
    PluginResource::GeneratedFile {
        destination: destination.to_owned(),
        contents: bytes.to_vec(),
    }
}

fn all_resources() -> Vec<PluginResource> {
    vec![
        generated("${install}/generated.bin", b"generated"),
        PluginResource::Shortcut {
            location: ShortcutLocation::Desktop,
            name: "Acme Plugin".to_owned(),
            target: "${install}/generated.bin".to_owned(),
            arguments: vec!["--plugin".to_owned()],
            working_directory: Some("${install}".to_owned()),
        },
        PluginResource::PathEntry {
            value: "${install}/plugin-bin".to_owned(),
        },
        PluginResource::Service {
            id: "plugin-service".to_owned(),
            name: "plugin-service".to_owned(),
            display_name: Some("Plugin Service".to_owned()),
            binary: "${install}/generated.bin".to_owned(),
            arguments: vec!["--service".to_owned()],
            start: ServiceStart::Manual,
        },
        PluginResource::Protocol {
            scheme: "plugin".to_owned(),
            executable: "${install}/generated.bin".to_owned(),
            args: vec!["%1".to_owned()],
        },
        PluginResource::FileType {
            extension: ".plugin".to_owned(),
            id: "Acme.Plugin".to_owned(),
            description: Some("Plugin document".to_owned()),
            executable: "${install}/generated.bin".to_owned(),
        },
    ]
}

#[test]
fn generated_file_modifies_the_normal_plan_and_returns_overlay_bytes() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(vec![generated("${install}/generated.txt", b"hello")]),
    )]);
    let result =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap();

    assert_eq!(result.plan.files.len(), 1);
    let generated_file = result
        .plan
        .files
        .iter()
        .find(|file| file.destination.to_string().ends_with("/generated.txt"))
        .unwrap();
    assert!(
        generated_file
            .source_relative
            .as_str()
            .starts_with("__zup_plugins__/")
    );
    assert!(!generated_file.source_relative.as_str().contains("helper"));
    assert!(generated_file.source_relative.as_str().ends_with(".bin"));
    assert_eq!(generated_file.size, 5);
    assert_eq!(generated_file.privilege, Privilege::User);
    assert_eq!(result.plan.summary.install_bytes, 5);
    assert_eq!(result.generated_files.len(), 1);
    assert_eq!(result.generated_files[0].bytes, b"hello");

    let mut hasher = Sha256::new();
    hasher.update(b"hello");
    let digest = zup_core::Sha256Digest::from_hasher(hasher);
    assert_eq!(generated_file.sha256, digest);
    assert_eq!(result.generated_files[0].sha256, digest);
}

#[test]
fn all_managed_resource_families_reach_the_install_plan() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(all_resources()),
    )]);
    let result =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap();

    assert_eq!(result.plan.files.len(), 1);
    assert_eq!(result.plan.shortcuts.len(), 1);
    assert_eq!(result.plan.path_entries.len(), 1);
    assert_eq!(result.plan.services.len(), 1);
    assert_eq!(result.plan.protocols.len(), 1);
    assert_eq!(result.plan.file_types.len(), 1);
    assert_eq!(result.plan.services[0].privilege, Privilege::Machine);
    assert_eq!(result.plan.shortcuts[0].privilege, Privilege::User);
    assert_eq!(result.plan.file_types[0].privilege, Privilege::User);
}

#[test]
fn plugin_component_and_condition_are_both_required() {
    let build = build(&with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "extra"
name = "Extra"
default = false
requires = ["core"]

[[plugins]]
id = "helper"
source = "plugins/one.wasm"
component = "extra"
when = 'component("extra")'
"#,
    ));
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(vec![generated("${install}/plugin.txt", b"x")]),
    )]);

    let without = plan_with_plugins(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: zup_plan::ComponentOverrides::none(),
        },
        facts(),
        &mut executor,
        &NeverCancelled,
    )
    .unwrap();
    assert!(
        without
            .plan
            .files
            .iter()
            .all(|file| !file.destination.to_string().ends_with("/plugin.txt"))
    );
    assert_eq!(executor.calls.len(), 0);

    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(vec![generated("${install}/plugin.txt", b"x")]),
    )]);
    let with_extra = plan_with_plugins(
        &build,
        &PlanRequest {
            scope: SelectedScope::User,
            install_directory: None,
            components: zup_plan::ComponentOverrides {
                enable: [component_id("extra")].into_iter().collect(),
                ..Default::default()
            },
        },
        facts(),
        &mut executor,
        &NeverCancelled,
    )
    .unwrap();
    assert!(
        with_extra
            .plan
            .files
            .iter()
            .any(|file| file.destination.to_string().ends_with("/plugin.txt"))
    );
    assert_eq!(executor.calls.len(), 1);
}

#[test]
fn typed_host_facts_are_passed_through_without_string_validation() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let host = PluginHostFacts::new(PluginOperatingSystem::Linux, PluginArchitecture::Aarch64);
    let mut executor =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::default())]);
    plan_with_plugins(&build, &request(), host, &mut executor, &NeverCancelled).unwrap();
    assert_eq!(executor.calls[0].1.host, host);
}

#[test]
fn ordinary_plan_rejects_an_active_plugin_declaration() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let error = plan(&build, &request()).expect_err("ordinary planner has no executor");
    assert!(matches!(
        error,
        PlanError::PluginPlanningRequired { ref plugin_id } if plugin_id.as_str() == "helper"
    ));
}

#[test]
fn executor_receives_the_exact_resolved_context_in_declaration_order() {
    let build = build(&with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "extra"
name = "Extra"
default = true
requires = ["core"]

[[plugins]]
id = "one"
source = "plugins/one.wasm"
component = "extra"

[[plugins]]
id = "two"
source = "plugins/two.wasm"
when = 'component("core")'
"#,
    ));
    let mut executor = FakeExecutor::new([
        ("one".to_owned(), PluginResourceProposal::default()),
        ("two".to_owned(), PluginResourceProposal::default()),
    ]);
    let host = PluginHostFacts::new(PluginOperatingSystem::Linux, PluginArchitecture::Aarch64);
    let result =
        plan_with_plugins(&build, &request(), host, &mut executor, &NeverCancelled).unwrap();
    assert_eq!(
        result.plan.selected_components,
        vec![component_id("core"), component_id("extra")]
    );
    let ids: Vec<_> = executor.calls.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["one", "two"]);
    let context = &executor.calls[0].1;
    assert_eq!(context.app.id.as_str(), "com.example.acme");
    assert_eq!(context.app.name.as_str(), "Acme");
    assert_eq!(context.app.version.to_string(), "1.4.0");
    assert_eq!(context.scope, SelectedScope::User);
    assert_eq!(
        context.install_directory.to_string(),
        "${known.local_app_data}/Programs/Acme"
    );
    assert_eq!(
        context.selected_components,
        vec![component_id("core"), component_id("extra")]
    );
    assert_eq!(context.host, host);
    assert!(executor.cancellation_seen);
}

#[test]
fn executor_failure_is_typed_and_stops_later_plugins() {
    let build = build(&with(
        r#"
[[plugins]]
id = "one"
source = "plugins/one.wasm"

[[plugins]]
id = "two"
source = "plugins/two.wasm"
"#,
    ));
    let mut executor = FakeExecutor::failing("one", PluginFailure::internal("boom"));
    let error = plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled)
        .expect_err("failure propagates");
    assert!(matches!(
        error,
        PlanError::PluginExecutionFailed {
            ref plugin_id,
            failure: PluginFailure::Internal { ref message },
        } if plugin_id.as_str() == "one" && message == "boom"
    ));
    assert_eq!(executor.calls.len(), 1);
}

#[test]
fn cancellation_is_checked_and_forwarded_without_a_clock() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let mut executor =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::default())]);
    let calls = AtomicUsize::new(0);
    let cancellation = || {
        calls.fetch_add(1, Ordering::Relaxed);
        false
    };
    plan_with_plugins(&build, &request(), facts(), &mut executor, &cancellation).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(executor.cancellation_seen);
}

#[test]
fn cancellation_before_a_plugin_returns_a_typed_error() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let mut executor = FakeExecutor::default();
    let error = plan_with_plugins(&build, &request(), facts(), &mut executor, &|| true)
        .expect_err("cancelled");
    assert!(matches!(
        error,
        PlanError::PluginCancelled { ref plugin_id } if plugin_id.as_str() == "helper"
    ));
    assert!(executor.calls.is_empty());
}

#[test]
fn duplicate_and_case_folded_collisions_are_rejected() {
    let cases = [
        (
            r#"
[[files]]
source = "base.txt"
destination = "${install}"
"#,
            generated("${install}/BASE.TXT", b"x"),
        ),
        (
            r#"
[[shortcuts]]
location = "start-menu"
name = "Same"
target = "${install}/base.txt"
"#,
            PluginResource::Shortcut {
                location: ShortcutLocation::StartMenu,
                name: "same".to_owned(),
                target: "${install}/base.txt".to_owned(),
                arguments: Vec::new(),
                working_directory: None,
            },
        ),
        (
            r#"
[[path]]
value = "${install}/Same"
"#,
            PluginResource::PathEntry {
                value: "${install}/same".to_owned(),
            },
        ),
        (
            r#"
[[services]]
id = "SameService"
name = "SameService"
binary = "${install}/base.txt"
start = "manual"
"#,
            PluginResource::Service {
                id: "sameservice".to_owned(),
                name: "sameservice".to_owned(),
                display_name: None,
                binary: "${install}/base.txt".to_owned(),
                arguments: Vec::new(),
                start: ServiceStart::Manual,
            },
        ),
        (
            r#"
[[protocols]]
scheme = "SameScheme"
executable = "${install}/base.txt"
"#,
            PluginResource::Protocol {
                scheme: "samescheme".to_owned(),
                executable: "${install}/base.txt".to_owned(),
                args: Vec::new(),
            },
        ),
        (
            r#"
[[file_types]]
extension = ".Same"
id = "Same.Type"
executable = "${install}/base.txt"
"#,
            PluginResource::FileType {
                extension: ".same".to_owned(),
                id: "same.type".to_owned(),
                description: None,
                executable: "${install}/base.txt".to_owned(),
            },
        ),
    ];

    for (manifest, proposal) in cases {
        let build = build(&with(&format!(
            "[[plugins]]\nid = \"helper\"\nsource = \"plugins/one.wasm\"\n{manifest}"
        )));
        let mut executor = FakeExecutor::new([(
            "helper".to_owned(),
            PluginResourceProposal::new(vec![proposal]),
        )]);
        let error = plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled)
            .expect_err("case-folded collision");
        assert!(matches!(error, PlanError::PluginResourceCollision { .. }));
    }
}

#[test]
fn duplicate_resources_within_and_across_plugins_are_rejected() {
    let build = build(&with(
        r#"
[[plugins]]
id = "one"
source = "plugins/one.wasm"

[[plugins]]
id = "two"
source = "plugins/two.wasm"
"#,
    ));
    let mut within = FakeExecutor::new([(
        "one".to_owned(),
        PluginResourceProposal::new(vec![
            generated("${install}/same.txt", b"a"),
            generated("${install}/SAME.txt", b"b"),
        ]),
    )]);
    let error = plan_with_plugins(&build, &request(), facts(), &mut within, &NeverCancelled)
        .expect_err("within-plugin collision");
    assert!(matches!(error, PlanError::PluginResourceCollision { .. }));

    let mut across = FakeExecutor::new([
        (
            "one".to_owned(),
            PluginResourceProposal::new(vec![generated("${install}/same.txt", b"a")]),
        ),
        (
            "two".to_owned(),
            PluginResourceProposal::new(vec![generated("${install}/same.txt", b"b")]),
        ),
    ]);
    let error = plan_with_plugins(&build, &request(), facts(), &mut across, &NeverCancelled)
        .expect_err("across-plugin collision");
    assert!(matches!(
        error,
        PlanError::PluginResourceCollision {
            ref plugin_id,
            ref existing_plugin_id,
            ..
        } if plugin_id.as_ref().is_some_and(|id| id.as_str() == "two")
            && existing_plugin_id.as_ref().is_some_and(|id| id.as_str() == "one")
    ));
}

#[test]
fn malformed_resources_are_rejected() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let cases = vec![
        PluginResource::PathEntry {
            value: "${unknown}".to_owned(),
        },
        PluginResource::FileType {
            extension: "plugin".to_owned(),
            id: "Plugin.Type".to_owned(),
            description: None,
            executable: "${install}/base.txt".to_owned(),
        },
        PluginResource::Service {
            id: "\0".to_owned(),
            name: "service".to_owned(),
            display_name: None,
            binary: "${install}/base.txt".to_owned(),
            arguments: Vec::new(),
            start: ServiceStart::Manual,
        },
    ];
    for resource in cases {
        let mut executor = FakeExecutor::new([(
            "helper".to_owned(),
            PluginResourceProposal::new(vec![resource]),
        )]);
        let error = plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled)
            .expect_err("malformed resource");
        assert!(matches!(
            error,
            PlanError::PluginResourceRejected { .. } | PlanError::PluginResourceLimit { .. }
        ));
    }
}

#[test]
fn plugin_destinations_reuse_windows_literal_validation() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let cases = vec![
        generated("${install}/file.", b"x"),
        PluginResource::Shortcut {
            location: ShortcutLocation::Desktop,
            name: "Bad".to_owned(),
            target: "${install}/CON".to_owned(),
            arguments: Vec::new(),
            working_directory: None,
        },
        PluginResource::Shortcut {
            location: ShortcutLocation::Desktop,
            name: "Bad work".to_owned(),
            target: "${install}/base.txt".to_owned(),
            arguments: Vec::new(),
            working_directory: Some("${install}/file:stream".to_owned()),
        },
        PluginResource::Service {
            id: "bad-service".to_owned(),
            name: "Bad service".to_owned(),
            display_name: None,
            binary: "${install}/file\\name.exe".to_owned(),
            arguments: Vec::new(),
            start: ServiceStart::Manual,
        },
        PluginResource::Protocol {
            scheme: "bad-protocol".to_owned(),
            executable: "${install}/file\u{0001}.exe".to_owned(),
            args: Vec::new(),
        },
        PluginResource::FileType {
            extension: ".bad".to_owned(),
            id: "Bad.Type".to_owned(),
            description: None,
            executable: "${install}/file?bad.exe".to_owned(),
        },
    ];
    for resource in cases {
        let mut executor = FakeExecutor::new([(
            "helper".to_owned(),
            PluginResourceProposal::new(vec![resource]),
        )]);
        let error = plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled)
            .expect_err("invalid Windows destination");
        assert!(matches!(error, PlanError::PluginResourceRejected { .. }));
    }
}

#[test]
fn generated_file_and_resource_limits_are_enforced() {
    let plugin_source = with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    );
    let build = build(&plugin_source);

    let too_large = vec![generated(
        "${install}/large.bin",
        &vec![0; MAX_PLUGIN_GENERATED_FILE_BYTES + 1],
    )];
    let mut executor =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::new(too_large))]);
    let error =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap_err();
    assert!(matches!(error, PlanError::PluginResourceLimit { .. }));

    let mut total = Vec::new();
    for index in 0..9 {
        total.push(generated(
            &format!("${{install}}/{index}.bin"),
            &vec![0; MAX_PLUGIN_GENERATED_FILE_BYTES],
        ));
    }
    let mut executor =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::new(total))]);
    let error =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap_err();
    assert!(matches!(error, PlanError::PluginResourceLimit { .. }));

    let too_many = (0..=MAX_PLUGIN_RESOURCES)
        .map(|index| PluginResource::PathEntry {
            value: format!("${{install}}/{index}"),
        })
        .collect();
    let mut executor =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::new(too_many))]);
    let error =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap_err();
    assert!(matches!(error, PlanError::PluginResourceLimit { .. }));

    let long_string = "x".repeat(MAX_PLUGIN_STRING_BYTES + 1);
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(vec![PluginResource::PathEntry { value: long_string }]),
    )]);
    let error =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap_err();
    assert!(matches!(error, PlanError::PluginResourceLimit { .. }));

    let too_many_args = (0..=MAX_PLUGIN_ARGUMENTS)
        .map(|index| index.to_string())
        .collect();
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(vec![PluginResource::Shortcut {
            location: ShortcutLocation::Desktop,
            name: "Too many".to_owned(),
            target: "${install}/base.txt".to_owned(),
            arguments: too_many_args,
            working_directory: None,
        }]),
    )]);
    let error =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap_err();
    assert!(matches!(error, PlanError::PluginResourceLimit { .. }));
}

#[test]
fn reversed_guest_order_has_the_same_plan_and_hashes() {
    let build = build(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"
"#,
    ));
    let mut resources = all_resources();
    resources.reverse();
    let mut first = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(resources.clone()),
    )]);
    let a = plan_with_plugins(&build, &request(), facts(), &mut first, &NeverCancelled).unwrap();
    let mut second =
        FakeExecutor::new([("helper".to_owned(), PluginResourceProposal::new(resources))]);
    let b = plan_with_plugins(&build, &request(), facts(), &mut second, &NeverCancelled).unwrap();
    assert_eq!(a.plan, b.plan);
    assert_eq!(a.generated_files, b.generated_files);
}

#[test]
fn generated_source_identity_does_not_expose_plugin_origin() {
    let build = build(&with(
        r#"
[[plugins]]
id = "one"
source = "plugins/one.wasm"

[[plugins]]
id = "two"
source = "plugins/two.wasm"
"#,
    ));
    let mut executor = FakeExecutor::new([
        (
            "one".to_owned(),
            PluginResourceProposal::new(vec![generated("${install}/one.txt", b"one")]),
        ),
        (
            "two".to_owned(),
            PluginResourceProposal::new(vec![generated("${install}/two.txt", b"two")]),
        ),
    ]);
    let result =
        plan_with_plugins(&build, &request(), facts(), &mut executor, &NeverCancelled).unwrap();
    assert!(
        !result.generated_files[0]
            .source_relative
            .as_str()
            .contains("one")
    );
    assert!(
        !result.generated_files[1]
            .source_relative
            .as_str()
            .contains("two")
    );
    assert_ne!(
        result.generated_files[0].source_relative,
        result.generated_files[1].source_relative
    );
}

#[test]
fn machine_scope_plugin_resources_are_privileged_and_require_elevation() {
    let source = BASE.replace("scope = \"either\"", "scope = \"machine\"");
    let source = format!("{source}\n[[plugins]]\nid = \"helper\"\nsource = \"plugins/one.wasm\"\n");
    let build = build(&source);
    let mut executor = FakeExecutor::new([(
        "helper".to_owned(),
        PluginResourceProposal::new(all_resources()),
    )]);
    let result = plan_with_plugins(
        &build,
        &PlanRequest::new(SelectedScope::Machine),
        facts(),
        &mut executor,
        &NeverCancelled,
    )
    .unwrap();
    assert!(result.plan.summary.requires_elevation);
    assert!(
        result
            .plan
            .files
            .iter()
            .all(|file| file.privilege == Privilege::Machine)
    );
    assert!(
        result
            .plan
            .shortcuts
            .iter()
            .all(|resource| resource.privilege == Privilege::Machine)
    );
    assert!(
        result
            .plan
            .path_entries
            .iter()
            .all(|resource| resource.privilege == Privilege::Machine)
    );
    assert!(
        result
            .plan
            .services
            .iter()
            .all(|resource| resource.privilege == Privilege::Machine)
    );
    assert!(
        result
            .plan
            .protocols
            .iter()
            .all(|resource| resource.privilege == Privilege::Machine)
    );
    assert!(
        result
            .plan
            .file_types
            .iter()
            .all(|resource| resource.privilege == Privilege::Machine)
    );
}
