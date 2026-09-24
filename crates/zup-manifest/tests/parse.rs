//! Parsing tests for `zup_manifest::parse`.

use std::path::PathBuf;

use rstest::rstest;
use semver::Version;
use zup_manifest::{
    App, AppId, Install, InstallDirectory, InstallScope, Manifest, ManifestError, NonEmptyString,
    SCHEMA_VERSION, Source, Template, Variable, parse, parse_and_compile,
};

fn minimal(scope: &str) -> String {
    format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[source]
directory = "dist"

[install]
scope = "{scope}"

[install.directory]
user = "${{known.local_app_data}}/Acme"
machine = "${{known.program_files}}/Acme"
"#
    )
}

fn without_section(src: &str, section: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    let header = format!("[{section}]");

    for line in src.lines() {
        if line.trim() == header {
            skipping = true;
            continue;
        }
        if skipping {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push(line.to_owned());
    }

    out.join("\n")
}

fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\0' => out.push_str("\\u0000"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(character),
        }
    }
    out.push('"');
    out
}

#[test]
fn complete_valid_manifest_defaults() {
    let src = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.2.3"

[source]
directory = "dist"

[install]
scope = "user"

[install.directory]
user = "${install}/App"

[[components]]
id = "core"
name = "Core"
"#;

    let manifest = parse(src).expect("valid");
    assert_eq!(manifest.schema, SCHEMA_VERSION);
    assert_eq!(manifest.app.id, AppId::new("com.example.acme").unwrap());
    assert_eq!(manifest.app.publisher, None);
    assert_eq!(manifest.app.main, None);
    assert_eq!(manifest.app.description, None);

    assert_eq!(manifest.components.len(), 1);
    let component = &manifest.components[0];
    assert!(!component.required);
    assert!(component.default);
    assert!(component.requires.is_empty());

    assert!(manifest.plugins.is_empty());
    assert!(manifest.files.is_empty());
    assert!(manifest.shortcuts.is_empty());
    assert!(manifest.path.is_empty());
    assert!(manifest.services.is_empty());
    assert!(manifest.protocols.is_empty());
    assert!(manifest.file_types.is_empty());
}

#[test]
fn valid_minimal_manifest() {
    let parsed = parse(&minimal("user")).expect("valid manifest");

    assert_eq!(
        parsed,
        Manifest {
            schema: SCHEMA_VERSION,
            ui: None,
            app: App {
                id: AppId::new("com.example.acme").unwrap(),
                name: NonEmptyString::new("Acme").unwrap(),
                version: Version::new(1, 0, 0),
                publisher: None,
                main: None,
                description: None,
            },
            source: Source {
                directory: PathBuf::from("dist"),
            },
            install: Install {
                scope: InstallScope::User,
                directory: InstallDirectory {
                    user: Some(Template::parse("${known.local_app_data}/Acme").unwrap()),
                    machine: Some(Template::parse("${known.program_files}/Acme").unwrap()),
                },
                allow_directory_override: false,
            },
            updates: None,
            components: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            shortcuts: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_types: Vec::new(),
        }
    );
}

#[test]
fn plugin_defaults() {
    let src = format!(
        "{}\n[[plugins]]\nid = \"acme.plugin_1-x\"\nsource = \"plugins/acme.wasm\"\n",
        minimal("user")
    );
    let manifest = parse(&src).expect("valid plugin");
    assert_eq!(manifest.plugins.len(), 1);
    let plugin = &manifest.plugins[0];
    assert_eq!(plugin.id.as_str(), "acme.plugin_1-x");
    assert_eq!(plugin.source, "plugins/acme.wasm");
    assert_eq!(plugin.component, None);
    assert_eq!(plugin.when, None);
}

#[test]
fn plugin_component_and_condition_are_parsed() {
    let src = format!(
        "{}\n[[plugins]]\nid = \"acme.plugin\"\nsource = \"plugins/acme.wasm\"\ncomponent = \"core\"\nwhen = 'component(\"core\")'\n",
        minimal("user")
    );
    let manifest = parse(&src).expect("valid plugin fields");
    let plugin = &manifest.plugins[0];
    assert_eq!(
        plugin.component.as_ref().map(|id| id.as_str()),
        Some("core")
    );
    assert_eq!(
        plugin.when.as_ref().map(ToString::to_string),
        Some("component(\"core\")".to_owned())
    );
}

#[test]
fn plugin_unknown_field_rejected() {
    let src = format!(
        "{}\n[[plugins]]\nid = \"acme.plugin\"\nsource = \"plugins/acme.wasm\"\nextra = true\n",
        minimal("user")
    );
    let err = parse(&src).expect_err("unknown plugin field");
    assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
}

#[rstest]
#[case::empty("")]
#[case::leading_dot("./plugin.wasm")]
#[case::parent("../plugin.wasm")]
#[case::embedded_parent("plugins/../plugin.wasm")]
#[case::embedded_dot("plugins/./plugin.wasm")]
#[case::empty_component("plugins//plugin.wasm")]
#[case::trailing_slash("plugins/")]
#[case::backslash("plugins\\plugin.wasm")]
#[case::posix_root("/plugin.wasm")]
#[case::windows_drive("C:/plugin.wasm")]
#[case::windows_drive_relative("C:plugin.wasm")]
#[case::unc_forward_slashes("//server/share/plugin.wasm")]
#[case::unc_backslashes("\\\\server\\share\\plugin.wasm")]
fn invalid_plugin_source(#[case] source: &str) {
    let src = format!(
        "{}\n[[plugins]]\nid = \"acme.plugin\"\nsource = {}\n",
        minimal("user"),
        toml_string(source)
    );
    let err = parse(&src).expect_err("invalid plugin source");
    assert!(
        matches!(err, ManifestError::InvalidPluginSource { ref path, .. } if path == source),
        "{err:?}"
    );
}

#[test]
fn invalid_plugin_source_rejects_nul() {
    let src = format!(
        r#"{}
[[plugins]]
id = "acme.plugin"
source = "plugins/\u0000plugin.wasm"
"#,
        minimal("user")
    );
    let err = parse(&src).expect_err("NUL plugin source");
    assert!(
        matches!(err, ManifestError::InvalidPluginSource { .. }),
        "{err:?}"
    );
}

#[rstest]
#[case::user("user", InstallScope::User)]
#[case::machine("machine", InstallScope::Machine)]
#[case::either("either", InstallScope::Either)]
fn valid_install_scope(#[case] scope: &str, #[case] expected: InstallScope) {
    let parsed = parse(&minimal(scope)).expect("valid scope");
    assert_eq!(parsed.install.scope, expected);
}

#[test]
fn optional_app_metadata() {
    let src = minimal("user").replace(
        r#"name = "Acme""#,
        r#"name = "Acme"
publisher = "Acme Inc."
main = "Acme.exe"
description = "Hello""#,
    );
    let manifest = parse(&src).unwrap();
    assert_eq!(
        manifest.app.publisher.as_ref().map(AsRef::as_ref),
        Some("Acme Inc.")
    );
    assert_eq!(
        manifest.app.main.as_ref().map(|t| t.to_string()),
        Some("Acme.exe".to_owned())
    );
    assert_eq!(manifest.app.description.as_deref(), Some("Hello"));
}

#[test]
fn ui_branding_is_optional_and_constrained() {
    let source = minimal("user").replace(
        "[install]",
        "[ui]\naccent = \"#2563eb\"\ntheme = \"dark\"\n\n[install]",
    );
    let manifest = parse_and_compile(&source).unwrap();
    let ui = manifest.ui.unwrap();
    assert_eq!(ui.accent.as_deref(), Some("#2563eb"));
    assert_eq!(ui.theme, zup_core::UiTheme::Dark);

    let invalid = source.replace("#2563eb", "blue");
    let error = parse_and_compile(&invalid).unwrap_err();
    assert!(matches!(error, ManifestError::InvalidUiAccent { .. }));
}

#[rstest]
#[case::app("app")]
#[case::source("source")]
#[case::install("install")]
fn missing_required_section(#[case] section: &str) {
    let src = without_section(&minimal("user"), section);
    let err = parse(&src).expect_err("missing section");
    assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
}

#[test]
fn unsupported_schema_version() {
    let src = minimal("user").replace("schema = 1", "schema = 2");
    let err = parse(&src).expect_err("unsupported schema");
    assert!(matches!(
        err,
        ManifestError::UnsupportedSchema { found: 2, .. }
    ));
}

#[test]
fn invalid_semver() {
    let src = minimal("user").replace("1.0.0", "not-a-version");
    let err = parse(&src).expect_err("invalid semver");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn empty_app_id() {
    let src = minimal("user").replace(r#"id = "com.example.acme""#, r#"id = """#);
    let err = parse(&src).expect_err("empty app id");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn empty_app_name() {
    let src = minimal("user").replace(r#"name = "Acme""#, r#"name = """#);
    let err = parse(&src).expect_err("empty app name");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn empty_source_directory() {
    let src = minimal("user").replace(r#"directory = "dist""#, r#"directory = """#);
    let err = parse(&src).expect_err("empty source directory");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn unknown_template_variable_rejected() {
    let src = minimal("user").replace("${known.local_app_data}/Acme", "${known.foo}/Acme");
    let err = parse(&src).expect_err("unknown variable");
    match &err {
        ManifestError::Invalid { message, .. } => {
            assert!(message.contains("known.foo"), "message: {message}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn malformed_template_rejected() {
    let src = minimal("user").replace("${known.local_app_data}/Acme", "${install/missing-close");
    let err = parse(&src).expect_err("malformed template");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[rstest]
#[case::top_level("publisher = \"Acme\"\n")]
#[case::app_field("extra = 1\n")]
#[case::component_field("id = \"core\"\nname = \"Core\"\nextra = true\n")]
fn unknown_field(#[case] injected: &str) {
    let src = match injected {
        "publisher = \"Acme\"\n" => {
            minimal("user").replace("schema = 1\n", "schema = 1\npublisher = \"Acme\"\n")
        }
        "extra = 1\n" => {
            minimal("user").replace("name = \"Acme\"\n", "name = \"Acme\"\nextra = 1\n")
        }
        _ => {
            let with_component = format!(
                "{}\n[[components]]\n{}",
                minimal("user"),
                injected.trim_end()
            );
            with_component
        }
    };

    let err = parse(&src).expect_err("unknown field");
    assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
}

#[rstest]
#[case::empty_id("id = \"\"\nname = \"Core\"\n")]
#[case::empty_name("id = \"core\"\nname = \"\"\n")]
fn invalid_component_field(#[case] component: &str) {
    let src = format!("{}\n[[components]]\n{}", minimal("user"), component);
    let err = parse(&src).expect_err("invalid component");
    assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
}

#[test]
fn condition_is_parsed_into_ast() {
    let src = format!(
        r#"{}

[[files]]
source = "**/*"
destination = "${{install}}"
when = 'component("cli")'
"#,
        minimal("user")
    );
    let manifest = parse(&src).expect("valid when");
    assert!(manifest.files[0].when.is_some());
}

#[test]
fn malformed_condition_rejected() {
    let src = format!(
        r#"{}

[[files]]
source = "**/*"
destination = "${{install}}"
when = 'component()'
"#,
        minimal("user")
    );
    let err = parse(&src).expect_err("bad condition");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn template_parts_are_structural() {
    let manifest = parse(&minimal("user")).unwrap();
    let directory = manifest
        .install
        .directory
        .user
        .as_ref()
        .expect("user directory");
    assert_eq!(
        directory.parts(),
        [
            zup_manifest::TemplatePart::Variable(Variable::KnownLocalAppData),
            zup_manifest::TemplatePart::Literal("/Acme".to_owned()),
        ]
    );
}
