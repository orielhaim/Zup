//! Authoring and configuration layer for zup.
//!
//! Parse and validate `zup.toml`, then compile it into platform-independent
//! [`zup_core::Installer`] IR:
//!
//! ```text
//! zup.toml → parse → validate → compile → Installer IR
//! ```
//!
//! Everything in this crate is pure and deterministic. Parsing and compilation
//! never touch the filesystem.

#![forbid(unsafe_code)]

mod compile;
mod error;
mod model;
mod parse;
mod plugin;
mod schema;

pub use compile::{compile, parse_and_compile, parse_and_compile_named};
pub use error::ManifestError;
pub use model::{Manifest, SCHEMA_VERSION, Updates};
pub use parse::{parse, parse_named};
pub use plugin::Plugin;
pub use schema::{schema, schema_json};

pub use zup_core::{
    App, AppId, Component, ComponentId, Condition, FileExtension, FileMapping, FileType,
    FileTypeId, Install, InstallDirectory, InstallScope, Installer, NonEmptyString, PathEntry,
    PluginBinding, PluginId, Privilege, Protocol, ProtocolScheme, Service, ServiceId, ServiceStart,
    Shortcut, ShortcutLocation, Source, Template, TemplateError, TemplatePart, ValueError,
    Variable,
};
