//! Authoring and configuration layer for zup.
//!
//! Parse and validate `zup.toml`, select one target profile, then compile it
//! into platform-independent [`zup_core::Installer`] IR:
//!
//! ```text
//! zup.toml → parse → select target → validate → compile → Installer IR
//! ```
//!
//! Schema 1 declares each build target under `[build.targets.<profile>]`:
//!
//! ```toml
//! schema = 1
//!
//! [build]
//!
//! [build.targets.windows-x64]
//! target = "x86_64-pc-windows-msvc"
//! source = { directory = "dist/windows-x64" }
//! frontend = "console"
//!
//! [[components]]
//! id = "windows"
//! name = "Windows"
//! targets = ["windows-x64"]
//! ```
//!
//! Missing or empty resource `targets` lists apply to every declared profile.
//! Everything in this crate is pure and deterministic. Parsing, selection, and
//! compilation never touch the filesystem.

#![forbid(unsafe_code)]

mod compile;
mod error;
mod model;
mod parse;
mod plugin;
mod schema;
mod target;

pub use compile::{compile, parse_and_compile, parse_and_compile_named};
pub use error::ManifestError;
pub use model::{Build, Manifest, SCHEMA_VERSION, Targeted, Updates};
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
