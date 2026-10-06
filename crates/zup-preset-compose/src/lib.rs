//! Which window an application presents, and what that window is given.
//!
//! `zup-artifact` owns the `.zupui` *format*. This crate owns the decision an
//! application makes with one: which package it presents, whether this machine
//! can present it, what settings it is given, and which files of the project
//! those settings name.
//!
//! It is a build-plane crate, and the whole of it exists because that answer is
//! needed in two places that must not disagree. A build composes an installer
//! around the window and has to be right about which bytes run. A developer
//! previewing their application has to see what they will actually ship, and a
//! preview that resolved the preset its own way would be a preview of something
//! nobody is going to install. So both ask this one function, and neither has a
//! second implementation to drift.
//!
//! It is deliberately not part of the developer CLI. A CLI is a place commands
//! are written down; a rule that a second command needs is not a rule the CLI
//! should own, because the second command cannot reach it without copying it.
//!
//! Nothing here runs a compiler, a preset, or Cargo. The package is proved from
//! its own bytes, which is the entire reason an application consumes a package
//! rather than a project.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use jsonschema::error::ValidationErrorKind;
use zup_artifact::preset::PresetPackageView;
use zup_core::{
    Installer, NonEmptyString, PresetAsset, PresetRuntime, ProjectPath, ResolvedAsset,
    TargetTriple, Ui,
};
use zup_preset_protocol::{Capabilities, HostOffers};

/// The largest one application-provided asset may be.
///
/// A logo, an icon, or a font. This is a statement about what a preset is given
/// to draw with, not about what an application may ship: payload files have no
/// such bound, and inventing one here would only move the question.
pub const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;

/// A preset package is not self-contained if its schema reaches outside itself.
const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// How a schema marks a value as a file Zup manages.
const ASSET_MARKER: &str = "x-zup-asset";

/// The root a setting is reported under.
const SETTINGS_ROOT: &str = "ui.settings";

/// Why an application cannot present the window it configured.
#[derive(Debug, thiserror::Error)]
pub enum PresetProblem {
    #[error("`{path}` is not a readable preset package: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("`{path}` does not verify: {reason}")]
    Damaged { path: String, reason: String },
    #[error("{reason}")]
    Incompatible { reason: String },
    #[error("`{name}` does not accept the configured settings: {problems}")]
    SettingsRejected { name: String, problems: String },
    #[error("`{name}` cannot use the configured asset `{setting}`: {reason}")]
    AssetRejected {
        name: String,
        setting: String,
        reason: String,
    },
    /// No package could be obtained at all, so nothing could be presented.
    ///
    /// One variant rather than one per source, because the sources are the
    /// caller's business: where the preset zup ships lives on this machine, and
    /// this says what a caller has to arrange.
    #[error("{reason}")]
    Unavailable { reason: String },
}

/// One thing wrong with the application's `[ui.settings]`, named the way the
/// author wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingProblem {
    /// Where in `zup.toml` the value is, so a diagnostic points at something the
    /// author can find rather than at a JSON pointer into somebody else's schema.
    pub path: String,
    pub message: String,
}

/// One `.zupui` chosen for one target, and everything derived from it.
///
/// The settings schema travels with the answer because a caller that shows the
/// window has to be able to re-check a document against it. Reading the schema
/// back out of the package would be a second read of the same verified bytes for
/// an answer this value already holds.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The package the answer came from, project-relative paths already resolved.
    pub package: PathBuf,
    /// The runtime model the installer carries.
    pub runtime: PresetRuntime,
    /// The resolved assets, ready to be materialized wherever a caller keeps them.
    pub assets: Vec<ResolvedAsset>,
    /// The target's native preset executable, as it will be launched.
    pub executable: Vec<u8>,
    /// The schema the settings were accepted against.
    pub settings_schema: serde_json::Value,
}

/// Which `.zupui` a target presents, and what that package implies.
///
/// The single question, and the single flow: open, verify, select the target,
/// check the protocol and capabilities, validate the settings against the
/// schema the package carries, resolve the assets the settings named - in that
/// order, so a corrupt package is refused before anything is read out of it.
/// This is the flow `zup preset inspect` already walks, because a consumer that
/// parsed the format differently from a publisher's inspector would be two
/// implementations of one format.
///
/// `shipped` is asked only when the application named no package, because
/// finding the preset zup ships costs a directory walk and an application that
/// chose its own window should not pay for the one it did not choose. An
/// application that names none is not misconfigured: it gets the preset Zup
/// ships, and that is the whole of its preset selection semantics.
pub fn resolve(
    ui: &Ui,
    project_root: &Path,
    installer: &Installer,
    target: &TargetTriple,
    shipped: &dyn Fn() -> Result<PathBuf, String>,
    policy: &dyn zup_platform::SourceFilePolicy,
) -> Result<Resolved, PresetProblem> {
    let package = package_for(ui, project_root, shipped)?;
    let settings =
        serde_json::to_value(&ui.settings).map_err(|error| PresetProblem::SettingsRejected {
            name: package.display().to_string(),
            problems: format!("[ui.settings] is not a table of values: {error}"),
        })?;
    let selected = select(&package, installer, target, &settings)?;
    let prepared = prepare(project_root, &selected, policy)?;
    Ok(Resolved {
        package,
        runtime: prepared.runtime,
        assets: prepared.assets,
        executable: prepared.executable,
        settings_schema: selected.settings_schema,
    })
}

/// The package a target presents, and where it came from.
///
/// A configured path is resolved against the project, because `zup.toml` is read
/// from the project and a path in it means the same thing from any working
/// directory. An application that named none gets the preset Zup ships, which
/// arrives through the toolchain like every other binary a build composes from.
/// After that choice the two are the same thing: a verified package, one target
/// binary selected from it, one composition path, one host.
pub fn package_for(
    ui: &Ui,
    project_root: &Path,
    shipped: &dyn Fn() -> Result<PathBuf, String>,
) -> Result<PathBuf, PresetProblem> {
    let Some(relative) = &ui.preset else {
        return shipped().map_err(|reason| PresetProblem::Unavailable {
            reason: format!(
                "a graphical installer needs a preset and this machine has none; stage one with \
                 `cargo xtask toolchain build`, or name a package in `[ui].preset`: {reason}"
            ),
        });
    };
    let path = project_root.join(relative.as_str());
    if !path.is_file() {
        return Err(PresetProblem::Unavailable {
            reason: format!(
                "`[ui].preset` names `{}`, which does not exist",
                relative.as_str()
            ),
        });
    }
    Ok(path)
}

/// One `.zupui` package chosen for one target, and everything a build needs from
/// it.
#[derive(Debug)]
pub struct Selected {
    pub name: String,
    pub version: semver::Version,
    pub protocol: u32,
    pub required_capabilities: Capabilities,
    /// The application's settings, already accepted by the package's own schema.
    pub settings: serde_json::Value,
    /// The schema they were accepted against.
    pub settings_schema: serde_json::Value,
    /// The settings names the package's schema marks as Zup-managed assets.
    pub asset_settings: Vec<String>,
    /// The target's native preset executable, as it will be launched.
    pub executable: Vec<u8>,
}

/// What one `.zupui` selection produced for a target plan.
#[derive(Debug)]
pub struct Prepared {
    /// The runtime model the installer carries.
    pub runtime: PresetRuntime,
    /// The resolved assets, to hand to materialization.
    pub assets: Vec<ResolvedAsset>,
    /// The preset executable's bytes.
    pub executable: Vec<u8>,
}

/// Read and verify a package, and select one target's binary from it.
///
/// The same reader `zup preset inspect` uses, and the same full verification: a
/// package is proven before a single byte of it is used for anything.
pub fn open(path: &Path) -> Result<(PresetPackageView, String), PresetProblem> {
    let bytes = std::fs::read(path).map_err(|error| PresetProblem::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let view = PresetPackageView::open(bytes).map_err(|error| PresetProblem::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    view.verify().map_err(|error| PresetProblem::Damaged {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let name = format!("{} {}", view.name(), view.version());
    Ok((view, name))
}

/// Choose a preset for one target and prove this host can present it.
///
/// `installer` is the compiled IR for this target, used for one thing: what
/// capabilities this application could ever provide. Whether *this* launch is a
/// fresh install or a maintenance session is not known at build time, so the
/// build asks the broader question and the host asks the exact one before it
/// launches anything.
pub fn select(
    package: &Path,
    installer: &Installer,
    target: &TargetTriple,
    settings: &serde_json::Value,
) -> Result<Selected, PresetProblem> {
    let (view, _name) = open(package)?;
    let offers = HostOffers::new(host_capabilities(installer));
    offers
        .check(view.package().ui_protocol, view.required_capabilities())
        .map_err(|error| PresetProblem::Incompatible {
            reason: error.to_string(),
        })?;

    let executable = view
        .binary_for(target)
        .map_err(|error| PresetProblem::Incompatible {
            reason: error.to_string(),
        })?;

    let name = view.name().to_owned();
    let problems = validate_settings(&view.package().settings_schema, settings);
    if !problems.is_empty() {
        return Err(PresetProblem::SettingsRejected {
            name,
            problems: problems
                .iter()
                .map(|problem| format!("{} {}", problem.path, problem.message))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }

    let asset_settings = asset_settings(&view.package().settings_schema);
    for setting in &asset_settings {
        let value = settings.get(setting).and_then(serde_json::Value::as_str);
        let Some(value) = value else {
            continue;
        };
        if let Err(error) = ProjectPath::new(value) {
            return Err(PresetProblem::AssetRejected {
                name,
                setting: setting.clone(),
                reason: error.to_string(),
            });
        }
    }

    Ok(Selected {
        name,
        version: view.version().clone(),
        protocol: view.wire_protocol(),
        required_capabilities: view.required_capabilities().clone(),
        settings: settings.clone(),
        settings_schema: view.package().settings_schema.clone(),
        asset_settings,
        executable,
    })
}

/// Resolve the assets a selected preset's settings named, and the runtime model
/// that describes the selection to the installer.
pub fn prepare(
    project_root: &Path,
    selected: &Selected,
    policy: &dyn zup_platform::SourceFilePolicy,
) -> Result<Prepared, PresetProblem> {
    let mut assets = Vec::with_capacity(selected.asset_settings.len());
    let mut declared = Vec::with_capacity(selected.asset_settings.len());

    for setting in &selected.asset_settings {
        let Some(value) = selected
            .settings
            .get(setting)
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let written = ProjectPath::new(value).map_err(|error| PresetProblem::AssetRejected {
            name: selected.name.clone(),
            setting: setting.clone(),
            reason: error.to_string(),
        })?;
        let relative = written
            .to_relative()
            .map_err(|error| PresetProblem::AssetRejected {
                name: selected.name.clone(),
                setting: setting.clone(),
                reason: error.to_string(),
            })?;
        let (path, size, sha256) = zup_build::resolve_project_source(
            project_root,
            &relative,
            "preset asset",
            MAX_ASSET_BYTES,
            policy,
        )
        .map_err(|error| PresetProblem::AssetRejected {
            name: selected.name.clone(),
            setting: setting.clone(),
            reason: error.to_string(),
        })?;
        // Two names for identical bytes are one stored blob. The names stay
        // separate because a preset asks for them by name, but the content is
        // addressed once, which is what makes deduplication free.
        if !assets
            .iter()
            .any(|asset: &ResolvedAsset| asset.sha256 == sha256)
        {
            assets.push(ResolvedAsset {
                name: asset_name(setting)?,
                source: Some(path),
                source_relative: Some(relative),
                size,
                sha256,
            });
        }
        declared.push(PresetAsset {
            name: asset_name(setting)?,
            size,
            sha256,
        });
    }

    declared.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Prepared {
        runtime: PresetRuntime {
            name: NonEmptyString::new(selected.name.clone()).map_err(|error| {
                PresetProblem::Incompatible {
                    reason: error.to_string(),
                }
            })?,
            version: selected.version.clone(),
            protocol: selected.protocol,
            required_capabilities: selected
                .required_capabilities
                .names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            settings: selected.settings.clone(),
            assets: declared,
        },
        assets,
        executable: selected.executable.clone(),
    })
}

fn asset_name(setting: &str) -> Result<NonEmptyString, PresetProblem> {
    NonEmptyString::new(setting).map_err(|error| PresetProblem::Incompatible {
        reason: error.to_string(),
    })
}

/// What this application can offer a preset, across every surface it has.
///
/// The build's question. A host asks the narrower one for the launch it is
/// actually doing, because `maintenance` is a fact about the session rather than
/// about the application; the build cannot know it and must not pretend to.
pub fn host_capabilities(installer: &Installer) -> Capabilities {
    zup_artifact::preset::offers(installer)
}

/// The settings names this schema marks as Zup-managed assets.
///
/// Read from the package's own schema, because the marker is how a preset says
/// "this value is a file I want" without zup knowing anything about the preset.
/// A property counts when it carries the marker directly, or refers to a
/// definition that does - which is how `Option<AssetRef>` arrives, as an
/// `anyOf` of a `$ref` and a null.
pub fn asset_settings(schema: &serde_json::Value) -> Vec<String> {
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let mut found: Vec<String> = properties
        .iter()
        .filter(|(_, property)| marks_asset(schema, property, 0))
        .map(|(name, _)| name.clone())
        .collect();
    found.sort();
    found
}

/// Whether one subschema is an asset reference, following local `$ref`s.
fn marks_asset(root: &serde_json::Value, schema: &serde_json::Value, depth: u8) -> bool {
    if depth > MAX_REF_DEPTH {
        return false;
    }
    if schema
        .get(ASSET_MARKER)
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return true;
    }
    for keyword in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(branches) = schema.get(keyword).and_then(serde_json::Value::as_array)
            && branches
                .iter()
                .any(|branch| marks_asset(root, branch, depth + 1))
        {
            return true;
        }
    }
    match schema.get("$ref").and_then(serde_json::Value::as_str) {
        Some(reference) => local_definition(root, reference)
            .is_some_and(|resolved| marks_asset(root, &resolved, depth + 1)),
        None => false,
    }
}

/// The definition a local `#/$defs/<name>` reference names.
fn local_definition(root: &serde_json::Value, reference: &str) -> Option<serde_json::Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    root.get("$defs")?.get(name).cloned()
}

/// How deep a `$ref` chain may be walked before it is treated as not-an-asset.
///
/// A cycle in a schema is a schema no validator can compile either, and this is
/// not the place that reports it. Bounding the walk keeps a hostile or simply
/// broken schema from making this function recurse without end.
const MAX_REF_DEPTH: u8 = 16;

/// Check the application's settings against the schema the package carries.
///
/// The schema is self-contained by construction - a preset generates it from its
/// own types - and this refuses any that is not, rather than resolving a
/// reference the application could have pointed anywhere. A `[ui.settings]`
/// value must not be able to make a build read a file off the machine or make
/// a request over a network.
pub fn validate_settings(
    schema: &serde_json::Value,
    settings: &serde_json::Value,
) -> Vec<SettingProblem> {
    if let Some(problem) = external_reference(schema) {
        return vec![problem];
    }
    let Ok(validator) = jsonschema::options()
        .offline()
        .should_validate_formats(false)
        .build(schema)
    else {
        return vec![SettingProblem {
            path: SETTINGS_ROOT.to_owned(),
            message: format!(
                "the package's settings schema is not a schema this build can use; it declares \
                 dialect {SCHEMA_DIALECT}"
            ),
        }];
    };
    validator
        .iter_errors(settings)
        .flat_map(problems_of)
        .collect()
}

/// One validator error, as the problems a person can act on.
///
/// A schema that refuses unknown properties reports them as a set against the
/// object, not one error per name, so a misspelled setting would otherwise be
/// reported against `[ui.settings]` with the name only in the prose. Splitting
/// them is what makes a diagnostic point at the line to edit.
fn problems_of(error: jsonschema::ValidationError<'_>) -> Vec<SettingProblem> {
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => unexpected
            .iter()
            .map(|name| SettingProblem {
                path: format!("{SETTINGS_ROOT}.{name}"),
                message: error.to_string(),
            })
            .collect(),
        _ => vec![SettingProblem {
            path: settings_path(error.instance_path().to_string()),
            message: error.to_string(),
        }],
    }
}

/// A schema that reaches outside itself, named as the problem it is.
fn external_reference(schema: &serde_json::Value) -> Option<SettingProblem> {
    let mut found = None;
    visit_references(schema, &mut |reference| {
        if found.is_none() && !reference.starts_with('#') {
            found = Some(SettingProblem {
                path: SETTINGS_ROOT.to_owned(),
                message: format!(
                    "the package's settings schema refers to `{reference}`; a preset's schema \
                     must be self-contained"
                ),
            });
        }
    });
    found
}

fn visit_references(schema: &serde_json::Value, visit: &mut impl FnMut(&str)) {
    match schema {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                if name == "$ref"
                    && let Some(reference) = value.as_str()
                {
                    visit(reference);
                }
                visit_references(value, visit);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                visit_references(item, visit);
            }
        }
        _ => {}
    }
}

/// A JSON pointer to a path the author wrote in `zup.toml`.
fn settings_path(pointer: String) -> String {
    if pointer.is_empty() || pointer == "/" {
        return SETTINGS_ROOT.to_owned();
    }
    let mut path = String::from(SETTINGS_ROOT);
    for segment in pointer.trim_start_matches('/').split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        path.push('.');
        path.push_str(&segment);
    }
    path
}
