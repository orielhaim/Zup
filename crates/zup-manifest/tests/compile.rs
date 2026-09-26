//! Compilation and semantic validation tests.

use std::fs;

use rstest::rstest;
use zup_core::{ComponentId, Frontend, InstallScope, Installer, ServiceId, TargetTriple};
use zup_manifest::{
    Manifest, ManifestError, TargetOverrides, compile, parse, parse_and_compile, select_targets,
};

fn fixture() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/acme.toml"
    ))
    .expect("fixture")
}

fn base() -> String {
    r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
machine = "${location.programs}/Acme"
"#
    .to_owned()
}

fn with(src: &str) -> String {
    format!("{}\n{}", base(), src.trim_start())
}

fn compile_selected(manifest: &Manifest) -> Result<Installer, ManifestError> {
    let overrides = TargetOverrides::default();
    let targets = select_targets(manifest, &["windows-x64"], &overrides).unwrap();
    compile(manifest, &targets[0], &overrides)
}

#[test]
fn fixture_compiles_to_ir() {
    let installer = parse_and_compile(&fixture(), "windows-x64").expect("fixture compiles");

    assert_eq!(installer.app.id.as_str(), "com.acme.acme");
    assert_eq!(
        installer.target,
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    );
    assert_eq!(installer.app.version.to_string(), "1.4.0");
    assert_eq!(installer.frontend, Frontend::Gui);
    assert_eq!(installer.install.scope, InstallScope::Either);
    assert_eq!(installer.components.len(), 3);
    assert_eq!(installer.plugins.len(), 1);
    assert_eq!(installer.plugins[0].id.as_str(), "setup-helper");
    assert_eq!(installer.files.len(), 1);
    assert_eq!(installer.launchers.len(), 1);
    assert_eq!(installer.path.len(), 1);
    assert_eq!(installer.services.len(), 1);
    assert_eq!(installer.protocols.len(), 1);
    assert_eq!(installer.file_associations.len(), 1);

    assert_eq!(
        installer.components[0].id,
        ComponentId::new("core").unwrap()
    );
    assert!(installer.components[0].required);
    assert!(installer.components[0].default);

    assert_eq!(
        installer.services[0].id,
        ServiceId::new("acme-agent").unwrap()
    );
}

#[test]
fn compilation_is_deterministic() {
    let a = parse_and_compile(&fixture(), "windows-x64").unwrap();
    let b = parse_and_compile(&fixture(), "windows-x64").unwrap();
    assert_eq!(a, b);
}

#[test]
fn plugin_compiles_without_build_time_source() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[plugins]]
id = "setup.helper"
source = "plugins/setup-helper.wasm"
component = "core"
when = 'component("core")'
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").expect("valid plugin");
    assert_eq!(installer.plugins.len(), 1);
    let plugin = &installer.plugins[0];
    assert_eq!(plugin.id.as_str(), "setup.helper");
    assert_eq!(
        plugin.component.as_ref().map(|id| id.as_str()),
        Some("core")
    );
    assert_eq!(
        plugin.when.as_ref().map(ToString::to_string),
        Some("component(\"core\")".to_owned())
    );
    let json = serde_json::to_string(&installer).unwrap();
    assert!(!json.contains("plugins/setup-helper.wasm"), "json: {json}");
}

#[test]
fn duplicate_plugin() {
    let src = with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/one.wasm"

[[plugins]]
id = "helper"
source = "plugins/two.wasm"
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::DuplicatePlugin { ref id, .. } if id == "helper"
    ));
}

#[test]
fn plugin_ids_are_case_insensitive_identities() {
    let src = with(
        r#"
[[plugins]]
id = "Helper"
source = "plugins/one.wasm"

[[plugins]]
id = "helper"
source = "plugins/two.wasm"
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::DuplicatePlugin { ref id, .. } if id == "helper"
    ));
}

#[test]
fn invalid_plugin_source_is_rejected_by_compile() {
    let mut manifest = parse(&with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
    ))
    .unwrap();
    manifest.plugins[0].value.source = "../helper.wasm".to_owned();
    let err = compile_selected(&manifest).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::InvalidPluginSource { ref path, .. } if path == "../helper.wasm"
    ));
}

#[test]
fn unknown_plugin_component_reference() {
    let src = with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
component = "missing"
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::UnknownComponent { ref id, ref context, .. }
            if id == "missing" && context.contains("plugin")
    ));
}

#[test]
fn unknown_plugin_condition_reference() {
    let src = with(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
when = 'component("missing")'
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::UnknownComponent { ref id, ref context, .. }
            if id == "missing" && context.contains("plugin")
    ));
}

#[test]
fn plugin_declaration_order_is_preserved() {
    let src = with(
        r#"
[[plugins]]
id = "z-plugin"
source = "plugins/z.wasm"

[[plugins]]
id = "a-plugin"
source = "plugins/a.wasm"
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").unwrap();
    let ids: Vec<_> = installer
        .plugins
        .iter()
        .map(|plugin| plugin.id.as_str().to_owned())
        .collect();
    assert_eq!(ids, ["z-plugin", "a-plugin"]);
}

#[test]
fn ir_serialization_roundtrip() {
    let installer = parse_and_compile(&fixture(), "windows-x64").unwrap();
    let json = serde_json::to_string_pretty(&installer).unwrap();
    let restored: Installer = serde_json::from_str(&json).unwrap();
    assert_eq!(installer, restored);
}

#[test]
fn preserves_declaration_order() {
    let src = with(
        r#"
[[components]]
id = "b"
name = "B"

[[components]]
id = "a"
name = "A"
requires = ["b"]
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").unwrap();
    let ids: Vec<_> = installer
        .components
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect();
    assert_eq!(ids, ["b", "a"]);
}

#[test]
fn normalizes_component_defaults() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"
required = true
default = true
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").unwrap();
    assert!(installer.components[0].required);
    assert!(installer.components[0].default);
    assert!(installer.components[0].requires.is_empty());
}

#[test]
fn duplicate_component() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "core"
name = "Also core"
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::DuplicateComponent { ref id, .. } if id == "core"
    ));
}

#[test]
fn duplicate_service() {
    let src = with(
        r#"
[[services]]
id = "svc"
name = "svc"
binary = "${install}/svc.exe"
start = "manual"

[[services]]
id = "svc"
name = "svc2"
binary = "${install}/svc2.exe"
start = "manual"
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::DuplicateService { ref id, .. } if id == "svc"
    ));
}

#[test]
fn required_component_cannot_default_disabled() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"
required = true
default = false
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::RequiredComponentDisabled { ref id, .. } if id == "core"
    ));
}

#[test]
fn missing_dependency() {
    let src = with(
        r#"
[[components]]
id = "cli"
name = "CLI"
requires = ["tools"]
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::UnknownComponent { ref id, ref context, .. }
            if id == "tools" && context.contains("cli")
    ));
}

#[test]
fn self_dependency() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"
requires = ["core"]
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::ComponentSelfDependency { ref id, .. } if id == "core"
    ));
}

#[test]
fn direct_cycle() {
    let src = with(
        r#"
[[components]]
id = "a"
name = "A"
requires = ["b"]

[[components]]
id = "b"
name = "B"
requires = ["a"]
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    match err {
        ManifestError::ComponentCycle { path, .. } => {
            assert!(path.contains('a'), "path: {path}");
            assert!(path.contains('b'), "path: {path}");
        }
        other => panic!("expected cycle, got {other:?}"),
    }
}

#[test]
fn multi_node_cycle() {
    let src = with(
        r#"
[[components]]
id = "a"
name = "A"
requires = ["b"]

[[components]]
id = "b"
name = "B"
requires = ["c"]

[[components]]
id = "c"
name = "C"
requires = ["a"]
"#,
    );
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    match err {
        ManifestError::ComponentCycle { path, .. } => {
            assert!(path.contains("a →"), "path: {path}");
        }
        other => panic!("expected cycle, got {other:?}"),
    }
}

#[rstest]
#[case::file(
    r#"
[[files]]
source = "**/*"
destination = "${install}"
component = "tools"
"#,
    "tools",
    "file"
)]
#[case::launcher(
    r#"
[[launchers]]
location = "desktop"
name = "App"
target = "${install}/a.exe"
component = "tools"
"#,
    "tools",
    "launcher"
)]
#[case::path(
    r#"
[[path]]
value = "${install}/bin"
component = "tools"
"#,
    "tools",
    "path"
)]
#[case::service(
    r#"
[[services]]
id = "s"
name = "s"
binary = "${install}/s.exe"
start = "manual"
component = "tools"
"#,
    "tools",
    "service"
)]
#[case::condition(
    r#"
[[path]]
value = "${install}/bin"
when = 'component("tools")'
"#,
    "tools",
    "condition"
)]
fn unknown_component_reference(#[case] body: &str, #[case] id: &str, #[case] context: &str) {
    let src = with(body);
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    match err {
        ManifestError::UnknownComponent {
            id: found,
            context: found_ctx,
            ..
        } => {
            assert_eq!(found, id);
            assert!(
                found_ctx.contains(context),
                "context: {found_ctx}, expected {context}"
            );
        }
        other => panic!("expected unknown component, got {other:?}"),
    }
}

#[test]
fn valid_component_references() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
requires = ["core"]

[[files]]
source = "**/*"
destination = "${install}"
component = "core"
when = 'component("cli") && !component("missing")'
"#,
    );
    // "missing" is referenced from the condition and must be rejected.
    let err = compile_selected(&parse(&src).unwrap()).unwrap_err();
    assert!(matches!(
        err,
        ManifestError::UnknownComponent { ref id, ref context, .. }
            if id == "missing" && context.contains("condition")
    ));
}

#[test]
fn valid_condition_and_component_refs_compile() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[components]]
id = "cli"
name = "CLI"
requires = ["core"]

[[files]]
source = "**/*"
destination = "${install}"
component = "core"
when = 'component("cli")'

[[path]]
value = "${install}/bin"
when = 'component("cli") || component("core")'
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").expect("valid refs");
    assert_eq!(installer.files.len(), 1);
    assert_eq!(installer.path.len(), 1);
}

#[rstest]
#[case::user_scope("user", Some("${location.user_data}/Acme"), None, true)]
#[case::user_scope_missing("user", None, None, false)]
#[case::user_scope_wrong_side("user", None, Some("${location.programs}/Acme"), false)]
#[case::machine_scope("machine", None, Some("${location.programs}/Acme"), true)]
#[case::machine_scope_missing("machine", None, None, false)]
#[case::either_scope(
    "either",
    Some("${location.user_data}/Acme"),
    Some("${location.programs}/Acme"),
    true
)]
#[case::either_missing_machine("either", Some("${location.user_data}/Acme"), None, false)]
#[case::either_missing_user("either", None, Some("${location.programs}/Acme"), false)]
fn install_directory_coverage(
    #[case] scope: &str,
    #[case] user: Option<&str>,
    #[case] machine: Option<&str>,
    #[case] ok: bool,
) {
    let mut directory = String::from("[install.directory]\n");
    if let Some(user) = user {
        directory.push_str(&format!("user = \"{user}\"\n"));
    }
    if let Some(machine) = machine {
        directory.push_str(&format!("machine = \"{machine}\"\n"));
    }

    let src = format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = {{ directory = "dist/windows-x64" }}

[install]
scope = "{scope}"

{directory}
"#
    );

    let result = parse_and_compile(&src, "windows-x64");
    if ok {
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    } else {
        assert!(matches!(
            result,
            Err(ManifestError::MissingInstallDirectory { .. })
        ));
    }
}

#[test]
fn non_existing_source_directory_still_compiles() {
    let src = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "does-not-exist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"

[[files]]
source = "**/*.exe"
destination = "${install}"
"#;
    let installer = parse_and_compile(src, "windows-x64").expect("pure compile ignores filesystem");
    assert_eq!(
        installer.target,
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    );
    assert_eq!(installer.files.len(), 1);
}

#[test]
fn prerequisite_declarations_are_typed_and_component_scoped() {
    let src = with(
        r#"
[[components]]
id = "core"
name = "Core"

[[prerequisites]]
id = "vc-x64"
name = "Visual C++ v14 Runtime"
component = "core"
target = "x64"
requirement = { kind = "runtime", id = "windows.vc.v14", version = ">=14.0.0" }
package = { type = "embedded", path = "prerequisites/vc.exe", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", size = 42 }
installer = { arguments = ["/install", "/quiet", "/norestart"] }
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").expect("valid prerequisite");
    assert_eq!(installer.prerequisites.len(), 1);
    assert_eq!(installer.prerequisites[0].id.as_str(), "vc-x64");
    assert_eq!(
        installer.prerequisites[0].target,
        zup_core::PrerequisiteArchitecture::X64
    );
    assert_eq!(
        installer.prerequisites[0].requirement.kind_name(),
        "runtime"
    );
    let zup_core::PrerequisiteRequirement::Runtime(runtime) =
        &installer.prerequisites[0].requirement
    else {
        panic!("runtime requirement");
    };
    assert_eq!(runtime.id.as_str(), "windows.vc.v14");
    assert_eq!(
        runtime.version,
        Some(semver::VersionReq::parse(">=14.0.0").unwrap())
    );
    assert_eq!(
        installer.prerequisites[0].installer.arguments,
        vec!["/install", "/quiet", "/norestart"]
    );
}

#[test]
fn installed_package_requirements_declare_opaque_product_identities() {
    let src = with(
        r#"
[[prerequisites]]
id = "desktop"
name = "Desktop runtime"
requirement = { kind = "installed_package", id = "{F3017226-FE2A-4295-8A7C-971BF3207148}", version = ">=120.0" }
package = { type = "embedded", path = "desktop.msi", sha256 = "4444444444444444444444444444444444444444444444444444444444444444", size = 1 }
"#,
    );
    let installer = parse_and_compile(&src, "windows-x64").expect("valid installed package");
    let zup_core::PrerequisiteRequirement::InstalledPackage(package) =
        &installer.prerequisites[0].requirement
    else {
        panic!("installed package requirement");
    };
    assert_eq!(
        package.id.as_str(),
        "{F3017226-FE2A-4295-8A7C-971BF3207148}"
    );
}

#[test]
fn requirement_kinds_carry_no_platform_implementation_details() {
    let src = with(
        r#"
[[prerequisites]]
id = "registry-probe"
name = "Registry probe"
requirement = { kind = "registry_value", hive = "local_machine", key = "SOFTWARE\\Acme", value = "Version" }
package = { type = "embedded", path = "probe.exe", sha256 = "5555555555555555555555555555555555555555555555555555555555555555", size = 1 }
"#,
    );
    assert!(parse_and_compile(&src, "windows-x64").is_err());
    let product_detector = with(
        r#"
[[prerequisites]]
id = "desktop"
name = "Desktop runtime"
requirement = { kind = "msi_product", product_code = "{F3017226-FE2A-4295-8A7C-971BF3207148}" }
package = { type = "embedded", path = "desktop.msi", sha256 = "4444444444444444444444444444444444444444444444444444444444444444", size = 1 }
"#,
    );
    assert!(parse_and_compile(&product_detector, "windows-x64").is_err());
    for kind in ["exe", "msi"] {
        let installer_kind = with(&format!(
            r#"
[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = {{ kind = "runtime", id = "windows.vc.v14" }}
package = {{ type = "embedded", path = "runtime.msi", sha256 = "6666666666666666666666666666666666666666666666666666666666666666", size = 1 }}
installer = {{ kind = "{kind}" }}
"#
        ));
        assert!(parse_and_compile(&installer_kind, "windows-x64").is_err());
    }
}

#[test]
fn remote_prerequisite_requires_https_and_digest() {
    let src = with(
        r#"
[[prerequisites]]
id = "webview2"
name = "WebView2 Evergreen Runtime"
requirement = { kind = "runtime", id = "windows.webview2.evergreen", version = ">=120.0" }
package = { type = "remote", url = "https://cdn.example.test/webview2.exe", filename = "webview2.exe", sha256 = "1111111111111111111111111111111111111111111111111111111111111111", size = 100 }
"#,
    );
    assert!(parse_and_compile(&src, "windows-x64").is_ok());
    let insecure = src.replace("https://", "http://");
    assert!(matches!(
        parse_and_compile(&insecure, "windows-x64"),
        Err(ManifestError::InvalidPrerequisite { .. })
    ));
    let unpinned = src.replace(
        "sha256 = \"1111111111111111111111111111111111111111111111111111111111111111\", ",
        "",
    );
    assert!(parse_and_compile(&unpinned, "windows-x64").is_err());
}

#[test]
fn prerequisite_rejects_duplicate_and_unknown_component_references() {
    let duplicate = with(
        r#"
[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime.exe", sha256 = "2222222222222222222222222222222222222222222222222222222222222222", size = 1 }

[[prerequisites]]
id = "runtime"
name = "Runtime again"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime2.exe", sha256 = "3333333333333333333333333333333333333333333333333333333333333333", size = 1 }
"#,
    );
    assert!(matches!(
        parse_and_compile(&duplicate, "windows-x64"),
        Err(ManifestError::DuplicatePrerequisite { .. })
    ));
    let case_duplicate = with(
        r#"
[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime.exe", sha256 = "2222222222222222222222222222222222222222222222222222222222222222", size = 1 }

[[prerequisites]]
id = "Runtime"
name = "Runtime again"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime2.exe", sha256 = "3333333333333333333333333333333333333333333333333333333333333333", size = 1 }
"#,
    );
    assert!(matches!(
        parse_and_compile(&case_duplicate, "windows-x64"),
        Err(ManifestError::DuplicatePrerequisite { .. })
    ));
    let unknown = with(
        r#"
[[prerequisites]]
id = "runtime"
name = "Runtime"
component = "missing"
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime.exe", sha256 = "2222222222222222222222222222222222222222222222222222222222222222", size = 1 }
"#,
    );
    assert!(parse_and_compile(&unknown, "windows-x64").is_err());
}

#[test]
fn missing_field_directories_for_user_only_scope_ok_with_user_template() {
    let src = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
"#;
    let installer = parse_and_compile(src, "windows-x64").unwrap();
    assert_eq!(installer.install.scope, InstallScope::User);
    assert!(installer.install.directory.machine.is_none());
}
