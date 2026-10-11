use schemars::{JsonSchema, schema_for};
use serde_json::json;
use zup_core::Prerequisite;
use zup_core::{
    App, Component, ComponentGroup, FileAssociation, FileMapping, Frontend, Install, Launcher,
    PathEntry, Protocol, Service,
};

use crate::icon::IconSetting;
use crate::{Build, Distribution, Plugin, Publish, SCHEMA_VERSION, Targeted, Updates};

#[allow(dead_code)]
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
struct SchemaManifest {
    schema: u32,
    app: SchemaApp,
    #[serde(default)]
    frontend: Frontend,
    #[serde(default)]
    ui: zup_core::Ui,
    build: Build,
    install: Install,
    #[serde(default)]
    prerequisites: Vec<Targeted<Prerequisite>>,
    #[serde(default)]
    updates: Option<Updates>,
    #[serde(default)]
    distribution: Option<Distribution>,
    #[serde(default)]
    publish: Option<Publish>,
    #[serde(default)]
    components: Vec<Targeted<Component>>,
    #[serde(default)]
    component_groups: Vec<Targeted<ComponentGroup>>,
    #[serde(default)]
    plugins: Vec<Targeted<Plugin>>,
    #[serde(default)]
    files: Vec<Targeted<FileMapping>>,
    #[serde(default)]
    launchers: Vec<Targeted<Launcher>>,
    #[serde(default)]
    path: Vec<Targeted<PathEntry>>,
    #[serde(default)]
    services: Vec<Targeted<Service>>,
    #[serde(default)]
    protocols: Vec<Targeted<Protocol>>,
    #[serde(default)]
    file_associations: Vec<Targeted<FileAssociation>>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
/// Application identity and display metadata.
struct SchemaApp {
    #[serde(flatten)]
    #[schemars(flatten)]
    app: App,
    #[serde(default)]
    icon: Option<IconSetting>,
}

pub fn schema() -> schemars::Schema {
    let mut schema = schema_for!(SchemaManifest);
    if let Some(object) = schema.as_object_mut() {
        if let Some(properties) = object
            .get_mut("properties")
            .and_then(|properties| properties.as_object_mut())
            && let Some(schema_version) = properties
                .get_mut("schema")
                .and_then(|schema| schema.as_object_mut())
        {
            schema_version.insert("const".into(), json!(SCHEMA_VERSION));
        }
        object.insert("$id".into(), json!("https://zup.dev/schema/zup.toml.json"));
        object.insert("title".into(), json!("zup installer manifest"));
        object.insert(
            "description".into(),
            json!("Declarative application installer manifest for zup."),
        );
        object.insert(
            "examples".into(),
            json!([{
                "schema": SCHEMA_VERSION,
                "app": {
                    "id": "com.example.acme",
                    "name": "Acme",
                    "version": "1.0.0"
                },
                "build": {
                    "targets": {
                        "windows-x64": {
                            "target": "x86_64-pc-windows-msvc",
                            "source": { "directory": "dist/windows-x64" },
                            "frontend": "console",
                            "install": {
                                "scope": "user",
                                "directory": {
                                    "user": "${location.user_data}/Acme"
                                }
                            }
                        }
                    }
                },
                "install": {
                    "scope": "either",
                    "allow_directory_override": true,
                    "directory": {
                        "user": "${location.user_data}/Acme",
                        "machine": "${location.programs}/Acme"
                    }
                }
            }]),
        );
    }
    schema
}

pub fn schema_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&schema())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prerequisite_requirements_expose_only_portable_kinds() {
        let value: serde_json::Value =
            serde_json::from_str(&super::schema_json().unwrap()).unwrap();
        let requirement = &value["$defs"]["PrerequisiteRequirement"];
        let kinds = requirement["oneOf"]
            .as_array()
            .expect("requirement variants")
            .iter()
            .map(|variant| variant["properties"]["kind"]["const"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(&kinds).unwrap(),
            serde_json::json!(["runtime", "installed_package", "file_version"])
        );
        for variant in requirement["oneOf"].as_array().unwrap() {
            let properties = variant["properties"].as_object().unwrap();
            match properties["kind"]["const"].as_str().unwrap() {
                "runtime" => assert!(properties.contains_key("id")),
                "installed_package" => assert!(properties.contains_key("id")),
                "file_version" => assert!(properties.contains_key("path")),
                other => panic!("unexpected requirement kind `{other}`"),
            }
            assert!(!properties.contains_key("product_code"));
            assert!(!properties.contains_key("hive"));
            assert!(!properties.contains_key("key"));
            assert!(!properties.contains_key("expected"));
            assert_eq!(variant["additionalProperties"], serde_json::json!(false));
        }
    }

    #[test]
    fn prerequisite_installers_cannot_select_a_package_format() {
        let value: serde_json::Value =
            serde_json::from_str(&super::schema_json().unwrap()).unwrap();
        let installer = value["$defs"]["PrerequisiteInstaller"]
            .as_object()
            .expect("installer schema is an object");
        assert_eq!(installer["additionalProperties"], serde_json::json!(false));
        let properties = installer["properties"]
            .as_object()
            .expect("installer properties");
        assert!(!properties.contains_key("kind"));
        for required in [
            "arguments",
            "success_exit_codes",
            "reboot_exit_codes",
            "privilege",
        ] {
            assert!(
                properties.contains_key(required),
                "`{required}` must stay authorable"
            );
        }
        let declared = value["$defs"]
            .as_object()
            .expect("definitions")
            .iter()
            .filter(|(_, schema)| {
                schema["properties"]["requirement"].is_object()
                    || schema["required"].as_array().is_some_and(|required| {
                        required.contains(&serde_json::json!("requirement"))
                    })
            })
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert!(!declared.is_empty(), "no prerequisite declaration found");
        for name in declared {
            let properties = value["$defs"][name]["properties"]
                .as_object()
                .expect("declared properties");
            assert!(properties.contains_key("requirement"));
            assert!(!properties.contains_key("detector"));
        }
        let serialized = serde_json::to_string(&value).unwrap();
        for banned in [
            "registry_value",
            "msi_product",
            "RegistryValue",
            "RegistryHive",
            "MsiProduct",
            "VisualCppV14",
            "WebView2Evergreen",
        ] {
            assert!(
                !serialized.contains(banned),
                "`{banned}` must not survive into the published schema"
            );
        }
    }
}
