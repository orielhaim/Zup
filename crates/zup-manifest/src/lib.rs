#![forbid(unsafe_code)]

mod compile;
mod error;
mod icon;
mod model;
mod parse;
mod plugin;
mod schema;
mod target;

pub use compile::{compile, parse_and_compile, parse_and_compile_named};
pub use error::ManifestError;
pub use icon::IconConfig;
pub use model::{
    ArtifactId, ArtifactKind, ArtifactMode, ArtifactProfile, Build, Distribution, DistributionHost,
    Github, GithubNotes, GithubTag, GithubWorkflow, Manifest, Publish, SCHEMA_VERSION, Targeted,
    Updates,
};
pub use parse::{parse, parse_named};
pub use plugin::Plugin;
pub use schema::{schema, schema_json};
pub use target::{ResourceKind, TargetOverrideSet, select_targets, select_targets_with};

pub use zup_core::{
    App, AppId, Component, ComponentId, Condition, FileAssociation, FileAssociationId,
    FileExtension, FileMapping, Frontend, Install, InstallDirectory, InstallLocation, InstallScope,
    Installer, Launcher, LauncherLocation, NonEmptyString, PathEntry, PluginBinding, PluginId,
    Privilege, Protocol, ProtocolScheme, ResolvedTargetConfig, Service, ServiceId, ServiceStart,
    Source, TargetOverrides, TargetProfile, TargetProfileId, TargetTriple, Template, TemplateError,
    TemplatePart, ValueError, Variable,
};
