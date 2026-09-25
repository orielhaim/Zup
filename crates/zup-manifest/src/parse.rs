//! Parse a `zup.toml` document into the authoring model.

use serde::Deserialize;
use serde_spanned::Spanned;
use zup_core::Prerequisite;
use zup_core::{
    App, Component, FileMapping, FileType, Frontend, Install, PathEntry, Protocol, Service,
    Shortcut, Source, UiBranding,
};

use crate::error::{ManifestError, named_source_named, source_span};
use crate::model::{Manifest, SCHEMA_VERSION};
use crate::plugin::{Plugin, is_valid_source};

/// Parse and field-validate a `zup.toml` manifest from source text.
///
/// Parsing is pure: it never touches the filesystem. Cross-reference and
/// graph checks run later in [`crate::compile`].
pub fn parse(source: &str) -> Result<Manifest, ManifestError> {
    parse_named(source, "zup.toml")
}

pub fn parse_named(source: &str, name: &str) -> Result<Manifest, ManifestError> {
    let raw: RawManifest =
        toml::from_str(source).map_err(|err| ManifestError::from_toml_named(err, source, name))?;

    let schema_span = raw.schema.span();
    let schema = raw.schema.into_inner();
    if schema != SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchema {
            supported: SCHEMA_VERSION,
            found: schema,
            src: Some(named_source_named(name, source)),
            span: Some(source_span(schema_span)),
        });
    }

    for plugin in &raw.plugins {
        if !is_valid_source(&plugin.source) {
            return Err(ManifestError::InvalidPluginSource {
                path: plugin.source.clone(),
                src: Some(named_source_named(name, source)),
                span: None,
            });
        }
    }

    Ok(Manifest {
        schema,
        app: raw.app,
        frontend: raw.frontend,
        ui: raw.ui,
        source: raw.source,
        install: raw.install,
        prerequisites: raw.prerequisites,
        updates: raw.updates,
        components: raw.components,
        plugins: raw.plugins,
        files: raw.files,
        shortcuts: raw.shortcuts,
        path: raw.path,
        services: raw.services,
        protocols: raw.protocols,
        file_types: raw.file_types,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: Spanned<u32>,
    app: App,
    #[serde(default)]
    frontend: Frontend,
    #[serde(default)]
    ui: Option<UiBranding>,
    source: Source,
    install: Install,
    #[serde(default)]
    prerequisites: Vec<Prerequisite>,
    updates: Option<crate::model::Updates>,
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
