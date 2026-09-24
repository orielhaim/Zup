use schemars::{JsonSchema, schema_for};
use serde_json::json;
use zup_core::{
    App, Component, FileMapping, FileType, Frontend, Install, PathEntry, Protocol, Service,
    Shortcut, Source,
};

use crate::{Plugin, Updates};

#[allow(dead_code)]
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
struct SchemaManifest {
    schema: u32,
    app: App,
    #[serde(default)]
    frontend: Frontend,
    #[serde(default)]
    ui: Option<zup_core::UiBranding>,
    source: Source,
    install: Install,
    #[serde(default)]
    updates: Option<Updates>,
    #[serde(default)]
    components: Vec<Component>,
    #[serde(default)]
    plugins: Vec<Plugin>,
    #[serde(default)]
    files: Vec<FileMapping>,
    #[serde(default)]
    shortcuts: Vec<Shortcut>,
    #[serde(default)]
    path: Vec<PathEntry>,
    #[serde(default)]
    services: Vec<Service>,
    #[serde(default)]
    protocols: Vec<Protocol>,
    #[serde(default)]
    file_types: Vec<FileType>,
}

pub fn schema() -> schemars::Schema {
    let mut schema = schema_for!(SchemaManifest);
    if let Some(object) = schema.as_object_mut() {
        object.insert("$id".into(), json!("https://zup.dev/schema/zup.toml.json"));
        object.insert("title".into(), json!("zup installer manifest"));
        object.insert(
            "description".into(),
            json!("Declarative application installer manifest for zup."),
        );
        object.insert(
            "examples".into(),
            json!([{
                "schema": 1,
                "app": {
                    "id": "com.example.acme",
                    "name": "Acme",
                    "version": "1.0.0"
                },
                "source": { "directory": "dist" },
                "install": {
                    "scope": "user",
                    "allow_directory_override": true,
                    "directory": {
                        "user": "${known.local_app_data}/Acme"
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
    fn schema_exposes_authoring_capabilities() {
        let value: serde_json::Value =
            serde_json::from_str(&super::schema_json().unwrap()).unwrap();
        assert_eq!(value["$id"], "https://zup.dev/schema/zup.toml.json");
        assert_eq!(value["properties"]["frontend"]["default"], "gui");
        assert_eq!(
            value["$defs"]["Frontend"]["enum"],
            serde_json::json!(["gui", "console", "headless"])
        );
        assert!(
            value["properties"]["ui"].is_object() || value["properties"]["ui"]["anyOf"].is_array()
        );
        assert!(value["$defs"]["Install"]["properties"]["allow_directory_override"].is_object());
    }
}
