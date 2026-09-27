//! Parse a `zup.toml` document into the authoring model.

use serde::Deserialize;
use serde_spanned::Spanned;
use zup_core::Prerequisite;
use zup_core::{
    App, Component, FileAssociation, FileMapping, Frontend, Install, Launcher, PathEntry, Protocol,
    Service, UiBranding,
};

use crate::error::{ManifestError, named_source_named, source_span};
use crate::model::{Build, Manifest, SCHEMA_VERSION, Targeted};
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

    let manifest = Manifest {
        schema,
        app: raw.app,
        frontend: raw.frontend,
        ui: raw.ui,
        build: raw.build,
        install: raw.install,
        prerequisites: raw.prerequisites,
        updates: raw.updates,
        distribution: raw.distribution,
        publish: raw.publish,
        components: raw.components,
        plugins: raw.plugins,
        files: raw.files,
        launchers: raw.launchers,
        path: raw.path,
        services: raw.services,
        protocols: raw.protocols,
        file_associations: raw.file_associations,
    };
    crate::target::validate_target_matrix(&manifest)
        .map_err(|error| error.with_source_named(source, name))?;
    crate::target::validate_target_references(&manifest)
        .map_err(|error| error.with_source_named(source, name))?;
    for plugin in &manifest.plugins {
        if !is_valid_source(&plugin.value.source) {
            return Err(ManifestError::InvalidPluginSource {
                path: plugin.value.source.clone(),
                src: Some(named_source_named(name, source)),
                span: None,
            });
        }
    }
    Ok(manifest)
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
    build: Build,
    install: Install,
    #[serde(default)]
    prerequisites: Vec<Targeted<Prerequisite>>,
    updates: Option<crate::model::Updates>,
    #[serde(default)]
    distribution: Option<crate::model::Distribution>,
    #[serde(default)]
    publish: Option<crate::model::Publish>,
    #[serde(default)]
    components: Vec<Targeted<Component>>,
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
