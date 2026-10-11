use std::ops::Range;
use std::sync::Arc;

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;
use zup_core::InstallScope;

use crate::target::ResourceKind;

type Src = Arc<NamedSource<String>>;

#[derive(Debug, Error, Diagnostic)]
pub enum ManifestError {
    #[error("{message}")]
    #[diagnostic(code(zup_manifest::invalid))]
    Invalid {
        message: String,
        #[source_code]
        src: Option<Src>,
        #[label("{message}")]
        span: Option<SourceSpan>,
    },

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

    #[error("the build target matrix must contain at least one profile")]
    #[diagnostic(
        code(zup_manifest::empty_target_matrix),
        help("declare a profile under [build.targets.<profile>]")
    )]
    EmptyTargetMatrix {
        #[source_code]
        src: Option<Src>,
        #[label("target matrix is empty")]
        span: Option<SourceSpan>,
    },

    #[error("{resource} references unknown target profile `{profile}`")]
    #[diagnostic(
        code(zup_manifest::unknown_target_profile_reference),
        help("use an exact profile id declared under [build.targets]")
    )]
    UnknownTargetProfileReference {
        resource: ResourceKind,
        profile: String,
        #[source_code]
        src: Option<Src>,
        #[label("unknown target profile `{profile}`")]
        span: Option<SourceSpan>,
    },

    #[error("artifact `{artifact}` includes no target profile")]
    #[diagnostic(
        code(zup_manifest::empty_artifact),
        help("list the profiles this artifact carries under `targets`")
    )]
    EmptyArtifact {
        artifact: String,
        #[source_code]
        src: Option<Src>,
        #[label("artifact includes no target profile")]
        span: Option<SourceSpan>,
    },

    #[error("artifact `{artifact}` is `kind = \"single\"` but includes {count} target profiles")]
    #[diagnostic(
        code(zup_manifest::artifact_target_count),
        help(
            "a single-target artifact carries exactly one profile, or use `kind = \"universal\"`"
        )
    )]
    ArtifactTargetCount {
        artifact: String,
        count: usize,
        #[source_code]
        src: Option<Src>,
        #[label("a single-target artifact carries one profile")]
        span: Option<SourceSpan>,
    },

    #[error("artifact `{artifact}` declares an empty channel")]
    #[diagnostic(
        code(zup_manifest::empty_artifact_channel),
        help("name the release channel the artifact follows, or remove it to pin a version")
    )]
    EmptyArtifactChannel {
        artifact: String,
        #[source_code]
        src: Option<Src>,
        #[label("channel is empty")]
        span: Option<SourceSpan>,
    },

    #[error("artifact `{artifact}` declares output `{output}`, which is not a file name")]
    #[diagnostic(
        code(zup_manifest::artifact_output),
        help("give a bare file name; the build places it beside the manifest")
    )]
    ArtifactOutput {
        artifact: String,
        output: String,
        #[source_code]
        src: Option<Src>,
        #[label("output is not a bare file name")]
        span: Option<SourceSpan>,
    },

    #[error("unknown target selector `{selector}`")]
    #[diagnostic(
        code(zup_manifest::unknown_target_selector),
        help("available target profiles: {available}")
    )]
    UnknownTargetSelector {
        selector: String,
        available: String,
        #[source_code]
        src: Option<Src>,
        #[label("unknown target selector")]
        span: Option<SourceSpan>,
    },

    #[error(
        "target profile `{profile}` duplicates canonical target `{target}` from profile `{conflicts_with}`"
    )]
    #[diagnostic(
        code(zup_manifest::duplicate_target),
        help("assign a distinct canonical target to each target profile")
    )]
    DuplicateTarget {
        profile: String,
        conflicts_with: String,
        target: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate canonical target")]
        span: Option<SourceSpan>,
    },

    #[error("invalid resolved target configuration for profile `{profile}`: {reason}")]
    #[diagnostic(
        code(zup_manifest::invalid_resolved_target_config),
        help("resolve the target again with select_targets")
    )]
    InvalidResolvedTargetConfig {
        profile: String,
        reason: String,
        #[source_code]
        src: Option<Src>,
        #[label("resolved target does not match its declaration")]
        span: Option<SourceSpan>,
    },

    #[error("duplicate component id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_component))]
    DuplicateComponent {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate component id")]
        span: Option<SourceSpan>,
    },

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

    #[error("component `{id}` cannot depend on itself")]
    #[diagnostic(code(zup_manifest::component_self_dependency))]
    ComponentSelfDependency {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("component depends on itself")]
        span: Option<SourceSpan>,
    },

    #[error("component dependency cycle: {path}")]
    #[diagnostic(code(zup_manifest::component_cycle))]
    ComponentCycle {
        path: String,
        #[source_code]
        src: Option<Src>,
        #[label("component dependency cycle")]
        span: Option<SourceSpan>,
    },

    #[error("required component `{id}` cannot default to disabled")]
    #[diagnostic(code(zup_manifest::required_component_disabled))]
    RequiredComponentDisabled {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("required component must be enabled")]
        span: Option<SourceSpan>,
    },

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

    #[error("duplicate protocol scheme `{scheme}`")]
    #[diagnostic(code(zup_manifest::duplicate_protocol))]
    DuplicateProtocol {
        scheme: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate protocol scheme")]
        span: Option<SourceSpan>,
    },

    #[error("duplicate file association id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_file_association))]
    DuplicateFileAssociation {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate file association id")]
        span: Option<SourceSpan>,
    },

    #[error("duplicate file association extension `{extension}`")]
    #[diagnostic(code(zup_manifest::duplicate_extension))]
    DuplicateExtension {
        extension: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate file association extension")]
        span: Option<SourceSpan>,
    },

    #[error("duplicate prerequisite id `{id}`")]
    #[diagnostic(code(zup_manifest::duplicate_prerequisite))]
    DuplicatePrerequisite {
        id: String,
        #[source_code]
        src: Option<Src>,
        #[label("duplicate prerequisite id")]
        span: Option<SourceSpan>,
    },

    #[error("invalid prerequisite `{id}`: {reason}")]
    #[diagnostic(code(zup_manifest::invalid_prerequisite))]
    InvalidPrerequisite {
        id: String,
        reason: String,
        #[source_code]
        src: Option<Src>,
        #[label("invalid prerequisite")]
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
            Self::EmptyTargetMatrix {
                span,
                src: existing,
            } => Self::EmptyTargetMatrix {
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::UnknownTargetProfileReference {
                resource,
                profile,
                span,
                src: existing,
            } => Self::UnknownTargetProfileReference {
                resource,
                profile,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::EmptyArtifact {
                artifact,
                span,
                src: existing,
            } => Self::EmptyArtifact {
                artifact,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::ArtifactTargetCount {
                artifact,
                count,
                span,
                src: existing,
            } => Self::ArtifactTargetCount {
                artifact,
                count,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::EmptyArtifactChannel {
                artifact,
                span,
                src: existing,
            } => Self::EmptyArtifactChannel {
                artifact,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::ArtifactOutput {
                artifact,
                output,
                span,
                src: existing,
            } => Self::ArtifactOutput {
                artifact,
                output,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::UnknownTargetSelector {
                selector,
                available,
                span,
                src: existing,
            } => Self::UnknownTargetSelector {
                selector,
                available,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::DuplicateTarget {
                profile,
                conflicts_with,
                target,
                span,
                src: existing,
            } => Self::DuplicateTarget {
                profile,
                conflicts_with,
                target,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::InvalidResolvedTargetConfig {
                profile,
                reason,
                span,
                src: existing,
            } => Self::InvalidResolvedTargetConfig {
                profile,
                reason,
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
            Self::DuplicateFileAssociation {
                id,
                span,
                src: existing,
            } => Self::DuplicateFileAssociation {
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
            Self::DuplicatePrerequisite {
                id,
                span,
                src: existing,
            } => Self::DuplicatePrerequisite {
                id,
                span: span.or(inferred),
                src: existing.or(src),
            },
            Self::InvalidPrerequisite {
                id,
                reason,
                span,
                src: existing,
            } => Self::InvalidPrerequisite {
                id,
                reason,
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
        ManifestError::EmptyTargetMatrix { .. } => {
            table_span(source, "build.targets").or_else(|| table_span(source, "build"))
        }
        ManifestError::UnknownTargetProfileReference { profile, .. } => source
            .find(profile.as_str())
            .map(|start| source_span(start..start + profile.len())),
        ManifestError::EmptyArtifact { .. } => {
            table_span(source, "build.artifacts").or_else(|| table_span(source, "build"))
        }
        ManifestError::ArtifactTargetCount { .. } => table_span(source, "targets"),
        ManifestError::EmptyArtifactChannel { .. } => value_span(source, "channel"),
        ManifestError::ArtifactOutput { output, .. } => source
            .find(output.as_str())
            .map(|start| source_span(start..start + output.len())),
        ManifestError::UnknownTargetSelector { .. } => None,
        ManifestError::DuplicateTarget { profile, .. }
        | ManifestError::InvalidResolvedTargetConfig { profile, .. } => source
            .find(profile.as_str())
            .map(|start| source_span(start..start + profile.len())),
        ManifestError::DuplicateComponent { id, .. }
        | ManifestError::UnknownComponent { id, .. }
        | ManifestError::ComponentSelfDependency { id, .. }
        | ManifestError::RequiredComponentDisabled { id, .. }
        | ManifestError::DuplicateService { id, .. }
        | ManifestError::DuplicatePlugin { id, .. }
        | ManifestError::DuplicatePrerequisite { id, .. }
        | ManifestError::InvalidPrerequisite { id, .. } => source
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
        ManifestError::DuplicateFileAssociation { id, .. } => source
            .find(id)
            .map(|start| source_span(start..start + id.len())),
        ManifestError::DuplicateExtension { extension, .. } => source
            .find(extension)
            .map(|start| source_span(start..start + extension.len())),
    }
}

fn table_span(source: &str, table: &str) -> Option<SourceSpan> {
    let header = format!("[{table}]");
    let start = source.find(&header)?;
    Some(source_span(start..start + header.len()))
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
