//! Manifest parse and validation diagnostics.

use std::ops::Range;
use std::sync::Arc;

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;
use zup_core::InstallScope;

type Src = Arc<NamedSource<String>>;

/// Errors produced while parsing, validating, or compiling a `zup.toml` manifest.
#[derive(Debug, Error, Diagnostic)]
pub enum ManifestError {
    /// The document is not a well-formed schema-1 manifest.
    #[error("{message}")]
    #[diagnostic(code(zup_manifest::invalid))]
    Invalid {
        message: String,
        #[source_code]
        src: Option<Src>,
        #[label("{message}")]
        span: Option<SourceSpan>,
    },

    /// `schema` is not a supported version.
    #[error("unsupported schema version `{found}`")]
    #[diagnostic(
        code(zup_manifest::unsupported_schema),
        help("supported schema version is `{supported}`")
    )]
    UnsupportedSchema {
        supported: u32,
        found: u32,
        #[source_code]
        src: Option<Src>,
        #[label("unsupported schema version")]
        span: Option<SourceSpan>,
    },

    #[error("UI accent must be a six-digit hex color such as #2563eb")]
    #[diagnostic(
        code(zup_manifest::invalid_ui_accent),
        help("use a value such as \"#2563eb\" or omit [ui].accent")
    )]
    InvalidUiAccent {
        #[source_code]
        src: Option<Src>,
        #[label("invalid UI accent")]
        span: Option<SourceSpan>,
    },

    /// Two components share an id.
    #[error("duplicate component id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_component))]
    DuplicateComponent {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate component id")]
        span: Option<SourceSpan>,
    },

    /// Two services share an id.
    #[error("duplicate service id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_service))]
    DuplicateService {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate service id")]
        span: Option<SourceSpan>,
    },

    #[error("duplicate plugin id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_plugin))]
    DuplicatePlugin {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate plugin id")]
        span: Option<SourceSpan>,
    },

    #[error("invalid plugin source `{path}`")]
    #[diagnostic(code(zup_manifest::invalid_plugin_source))]
    InvalidPluginSource {
        path: String,
        #[source_code]
        src: Option<Src>,
        #[label("invalid plugin source")]
        span: Option<SourceSpan>,
    },

    /// A resource or dependency references a component that does not exist.
    #[error("unknown component `{id}` referenced by {context}")]
    #[diagnostic(code(zup_manifest::unknown_component))]
    UnknownComponent {
        id: String,
        context: String,
        #[source_code]
        src: Option<Src>,
        #[label("unknown component `{id}`")]
        span: Option<SourceSpan>,
    },

    /// A component lists itself as a dependency.
    #[error("component `{id}` cannot depend on itself")]
    #[diagnostic(code(zup_manifest::component_self_dependency))]
    ComponentSelfDependency {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("component depends on itself")]
        span: Option<SourceSpan>,
    },

    /// Component dependencies contain a cycle.
    #[error("component dependency cycle: {path}")]
    #[diagnostic(code(zup_manifest::component_cycle))]
    ComponentCycle {
        path: String,
        #[source_code]
        src: Option<Src>,
        #[label("component dependency cycle")]
        span: Option<SourceSpan>,
    },

    /// A required component is marked as not selected by default.
    #[error("required component `{id}` cannot default to disabled")]
    #[diagnostic(code(zup_manifest::required_component_disabled))]
    RequiredComponentDisabled {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("required component must be enabled")]
        span: Option<SourceSpan>,
    },

    /// The install scope needs a directory template that was not configured.
    #[error("install directory for scope `{scope}` is not specified")]
    #[diagnostic(
        code(zup_manifest::missing_install_directory),
        help("set [install.directory] `user` and/or `machine` to cover the configured scope")
    )]
    MissingInstallDirectory {
        scope: InstallScope,
        #[source_code]
        src: Option<Src>,
        #[label("missing install directory")]
        span: Option<SourceSpan>,
    },

    /// Install directory templates must not reference `${install}`.
    #[error("install directory for scope `{scope}` must not reference `${{install}}`")]
    #[diagnostic(
        code(zup_manifest::recursive_install_directory),
        help("`${{install}}` expands to the install directory itself")
    )]
    RecursiveInstallDirectory {
        scope: InstallScope,
        #[source_code]
        src: Option<Src>,
        #[label("recursive install directory")]
        span: Option<SourceSpan>,
    },

    /// Two unconditional declarations share a protocol scheme.
    #[error("duplicate protocol scheme `{scheme}`")]
    #[diagnostic(code(zup_manifest::duplicate_protocol))]
    DuplicateProtocol {
        scheme: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate protocol scheme")]
        span: Option<SourceSpan>,
    },

    /// Two unconditional declarations share a file type id.
    #[error("duplicate file type id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_file_type))]
    DuplicateFileType {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate file type id")]
        span: Option<SourceSpan>,
    },

    /// Two unconditional declarations own the same file extension.
    #[error("duplicate file type extension `{extension}`")]
    #[diagnostic(code(zup_manifest::duplicate_extension))]
    DuplicateExtension {
        extension: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate file type extension")]
        span: Option<SourceSpan>,
    },
}

impl ManifestError {
    pub(crate) fn from_toml_named(err: toml::de::Error, source: &str, name: &str) -> Self {
        Self::Invalid {
            message: err.to_string(),
            src: Some(named_source_named(name, source)),
            span: err.span().map(source_span),
        }
    }

    pub fn with_source(self, source: &str) -> Self {
        self.with_source_named(source, "zup.toml")
    }

    pub fn with_source_named(self, source: &str, name: &str) -> Self {
        let src = Some(named_source_named(name, source));
        let inferred = infer_span(&self, source);
        match self {
            Self::Invalid {
                message,
                span,
                src: existing,
            } => Self::Invalid {
                message,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::InvalidUiAccent {
                span,
                src: existing,
            } => Self::InvalidUiAccent {
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::UnsupportedSchema {
                supported,
                found,
                span,
                src: existing,
            } => Self::UnsupportedSchema {
                supported,
                found,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateComponent {
                id,
                span,
                src: existing,
            } => Self::DuplicateComponent {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateService {
                id,
                span,
                src: existing,
            } => Self::DuplicateService {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicatePlugin {
                id,
                span,
                src: existing,
            } => Self::DuplicatePlugin {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::InvalidPluginSource {
                path,
                span,
                src: existing,
            } => Self::InvalidPluginSource {
                path,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::UnknownComponent {
                id,
                context,
                span,
                src: existing,
            } => Self::UnknownComponent {
                id,
                context,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::ComponentSelfDependency {
                id,
                span,
                src: existing,
            } => Self::ComponentSelfDependency {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::ComponentCycle {
                path,
                span,
                src: existing,
            } => Self::ComponentCycle {
                path,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::RequiredComponentDisabled {
                id,
                span,
                src: existing,
            } => Self::RequiredComponentDisabled {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::MissingInstallDirectory {
                scope,
                span,
                src: existing,
            } => Self::MissingInstallDirectory {
                scope,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::RecursiveInstallDirectory {
                scope,
                span,
                src: existing,
            } => Self::RecursiveInstallDirectory {
                scope,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateProtocol {
                scheme,
                span,
                src: existing,
            } => Self::DuplicateProtocol {
                scheme,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateFileType {
                id,
                span,
                src: existing,
            } => Self::DuplicateFileType {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateExtension {
                extension,
                span,
                src: existing,
            } => Self::DuplicateExtension {
                extension,
                span: span.or(inferred),
                src: existing.or(src),
            },
        }
    }
}

fn infer_span(error: &ManifestError, source: &str) -> Option<SourceSpan> {
    match error {
        ManifestError::Invalid { message, .. } => {
            if message.contains("[updates]") {
                value_span(source, "channel")
            } else {
                None
            }
        }
        ManifestError::UnsupportedSchema { .. } => value_span(source, "schema"),
        ManifestError::InvalidUiAccent { .. } => value_span(source, "accent"),
        ManifestError::DuplicateComponent { id, .. }
        | ManifestError::UnknownComponent { id, .. }
        | ManifestError::ComponentSelfDependency { id, .. }
        | ManifestError::RequiredComponentDisabled { id, .. }
        | ManifestError::DuplicateService { id, .. }
        | ManifestError::DuplicatePlugin { id, .. } => source
            .find(id.as_str())
            .map(|start| source_span(start..start + id.len())),
        ManifestError::InvalidPluginSource { path, .. } => source
            .find(path.as_str())
            .map(|start| source_span(start..start + path.len())),
        ManifestError::ComponentCycle { path, .. } => {
            let id = path.split(" → ").next()?;
            source
                .find(id)
                .map(|start| source_span(start..start + id.len()))
        }
        ManifestError::MissingInstallDirectory { .. }
        | ManifestError::RecursiveInstallDirectory { .. } => value_span(source, "scope"),
        ManifestError::DuplicateProtocol { scheme, .. } => source
            .find(scheme)
            .map(|start| source_span(start..start + scheme.len())),
        ManifestError::DuplicateFileType { id, .. } => source
            .find(id)
            .map(|start| source_span(start..start + id.len())),
        ManifestError::DuplicateExtension { extension, .. } => source
            .find(extension)
            .map(|start| source_span(start..start + extension.len())),
    }
}

fn value_span(source: &str, key: &str) -> Option<SourceSpan> {
    let key_start = source.find(key)?;
    let after_key = key_start + key.len();
    let equals = source[after_key..].find('=')? + after_key;
    let value_start = source[equals + 1..]
        .find(|character: char| !character.is_whitespace())
        .map(|offset| equals + 1 + offset)?;
    let line_end = source[value_start..]
        .find('\n')
        .map(|offset| value_start + offset)
        .unwrap_or(source.len());
    let value = source[value_start..line_end].trim_end();
    Some(source_span(value_start..value_start + value.len()))
}

pub(crate) fn named_source_named(name: &str, src: &str) -> Src {
    Arc::new(NamedSource::new(name, src.to_owned()))
}

pub(crate) fn source_span(range: Range<usize>) -> SourceSpan {
    let len = range.end.saturating_sub(range.start);
    SourceSpan::new(range.start.into(), len)
}

#[cfg(test)]
mod tests {
    use super::ManifestError;

    #[test]
    fn attaches_inferred_source_span() {
        let error = ManifestError::UnknownComponent {
            id: "cli-tools".into(),
            context: "file mapping".into(),
            src: None,
            span: None,
        }
        .with_source("component = \"cli-tools\"\n");
        let ManifestError::UnknownComponent { src, span, .. } = error else {
            panic!("unexpected diagnostic variant");
        };
        assert!(src.is_some());
        assert!(span.is_some());
    }
}
