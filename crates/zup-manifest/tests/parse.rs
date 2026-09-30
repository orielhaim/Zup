//! Parsing tests for `zup_manifest::parse`.

use rstest::rstest;
use zup_manifest::{AppId, Frontend, ManifestError, SCHEMA_VERSION, parse, parse_and_compile};

fn minimal(scope: &str) -> String {
    format!(
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

[install.directory]
user = "${{location.user_data}}/Acme"
machine = "${{location.programs}}/Acme"
"#
    )
}

fn without_section(src: &str, section: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    let header = format!("[{section}]");

    for line in src.lines() {
        let trimmed = line.trim_start();
        if section == "build" {
            if trimmed == "[build]" {
                skipping = true;
                continue;
            }
            if skipping {
                if trimmed.is_empty() || trimmed.starts_with("[build.") {
                    continue;
                }
                skipping = false;
            }
        } else if trimmed == header {
            skipping = true;
            continue;
        } else if skipping {
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

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

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
    let component = &manifest.components[0].value;
    assert!(!component.required);
    assert!(component.default);
    assert!(component.requires.is_empty());

    assert!(manifest.plugins.is_empty());
    assert!(manifest.files.is_empty());
    assert!(manifest.launchers.is_empty());
    assert!(manifest.path.is_empty());
    assert!(manifest.services.is_empty());
    assert!(manifest.protocols.is_empty());
    assert!(manifest.file_associations.is_empty());
}

#[test]
fn frontend_reaches_the_installer_from_the_manifest() {
    for (declaration, expected) in [(None, Frontend::Gui), (Some("console"), Frontend::Console)] {
        let mut source = minimal("user");
        if let Some(declaration) = declaration {
            source = source.replacen(
                "schema = 1\n",
                &format!("schema = 1\nfrontend = \"{declaration}\"\n"),
                1,
            );
        }
        let label = declaration.unwrap_or("absent");

        assert_eq!(parse(&source).expect("valid").frontend, expected, "{label}");
        assert_eq!(
            parse_and_compile(&source, "windows-x64")
                .expect("compiles")
                .frontend,
            expected,
            "{label}"
        );
    }
}

/// `[ui]` is a preset selection and that preset's own settings. There is no
/// second customization system: a preset author decides what a window looks
/// like, and the application fills in the values that preset declared.
#[test]
fn a_ui_section_names_a_preset_and_its_settings() {
    let source = minimal("user").replace(
        "[install]",
        "[ui]\npreset = \"./vendor/aurora.zupui\"\n\n[ui.settings]\naccent = \"#695cff\"\nhero = \"Install Acme\"\n\n[install]",
    );
    let manifest = parse(&source).expect("a preset selection parses");
    assert_eq!(
        manifest.ui.preset.as_ref().map(|path| path.as_str()),
        Some("vendor/aurora.zupui")
    );
    assert_eq!(
        manifest
            .ui
            .settings
            .get("accent")
            .and_then(|value| value.as_str()),
        Some("#695cff")
    );
    assert_eq!(
        manifest
            .ui
            .settings
            .get("hero")
            .and_then(|value| value.as_str()),
        Some("Install Acme")
    );
}

/// A preset path that leaves the project is refused by the type that holds it,
/// before anything tries to open it.
#[test]
fn a_preset_path_outside_the_project_is_refused() {
    for path in ["../elsewhere/aurora.zupui", "C:/elsewhere/aurora.zupui"] {
        let source = minimal("user").replace(
            "[install]",
            &format!("[ui]\npreset = \"{path}\"\n\n[install]"),
        );
        assert!(
            parse_and_compile(&source, "windows-x64").is_err(),
            "`{path}` names a file outside the project"
        );
    }
}

/// A `.zupui` is a build input, not something a manifest carries into a plan:
/// the resolved preset is attached by the build, from a verified package.
#[test]
fn the_compiled_installer_carries_no_preset_until_a_build_proves_one() {
    let source = minimal("user").replace(
        "[install]",
        "[ui]\npreset = \"./aurora.zupui\"\n\n[install]",
    );
    let installer = parse_and_compile(&source, "windows-x64").expect("a preset selection compiles");
    assert!(
        installer.preset.is_none(),
        "the manifest crate cannot verify a package, so it does not choose one"
    );
}

#[test]
fn missing_required_section_is_rejected() {
    for section in ["app", "build", "install"] {
        let src = without_section(&minimal("user"), section);
        let err = parse(&src).expect_err("missing section");
        assert!(
            matches!(err, ManifestError::Invalid { .. }),
            "{section}: {err:?}"
        );
    }
}

#[test]
fn unsupported_schema_version() {
    let src = minimal("user").replace("schema = 1", "schema = 2");
    let err = parse(&src).expect_err("unsupported schema");
    assert!(matches!(
        err,
        ManifestError::UnsupportedSchema {
            found: 2,
            supported: SCHEMA_VERSION,
            ..
        }
    ));
}

#[test]
fn invalid_semver() {
    let src = minimal("user").replace("1.0.0", "not-a-version");
    let err = parse(&src).expect_err("invalid semver");
    assert!(matches!(err, ManifestError::Invalid { .. }));
}

#[test]
fn empty_required_field_is_rejected() {
    let cases = [
        (r#"id = "com.example.acme""#, r#"id = """#),
        (r#"name = "Acme""#, r#"name = """#),
        (r#"directory = "dist/windows-x64""#, r#"directory = """#),
    ];
    for (authored, empty) in cases {
        let err = parse(&minimal("user").replace(authored, empty)).expect_err("empty field");
        assert!(
            matches!(err, ManifestError::Invalid { .. }),
            "{empty}: {err:?}"
        );
    }

    for (id, name) in [("", "Core"), ("core", "")] {
        let src = format!(
            "{}\n[[components]]\nid = \"{id}\"\nname = \"{name}\"\n",
            minimal("user")
        );
        let err = parse(&src).expect_err("empty component field");
        assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
    }
}

#[test]
fn template_authoring_errors_are_rejected() {
    let unknown = minimal("user").replace("${location.user_data}/Acme", "${known.foo}/Acme");
    match parse(&unknown).expect_err("unknown variable") {
        ManifestError::Invalid { ref message, .. } => {
            assert!(message.contains("known.foo"), "message: {message}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    let malformed =
        minimal("user").replace("${location.user_data}/Acme", "${install/missing-close");
    let err = parse(&malformed).expect_err("malformed template");
    assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
}

#[test]
fn unknown_field_is_rejected_wherever_it_appears() {
    let cases = [
        minimal("user").replace("schema = 1\n", "schema = 1\npublisher = \"Acme\"\n"),
        minimal("user").replace("name = \"Acme\"\n", "name = \"Acme\"\nextra = 1\n"),
        format!(
            "{}\n[[components]]\nid = \"core\"\nname = \"Core\"\nextra = true\n",
            minimal("user")
        ),
        format!(
            "{}\n[[plugins]]\nid = \"acme.plugin\"\nsource = \"plugins/acme.wasm\"\nextra = true\n",
            minimal("user")
        ),
        // A top-level `source` is the pre-`[build]` spelling and is not authorable.
        minimal("user").replace(
            "[build]\n\n[build.targets.windows-x64]\ntarget = \"x86_64-pc-windows-msvc\"\nsource = { directory = \"dist/windows-x64\" }",
            "[source]\ndirectory = \"dist\"",
        ),
    ];

    for src in cases {
        let err = parse(&src).expect_err("unknown field");
        assert!(matches!(err, ManifestError::Invalid { .. }), "{err:?}");
    }
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
#[case::nul("plugins/\0plugin.wasm")]
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
