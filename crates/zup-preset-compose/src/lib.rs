#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use jsonschema::error::ValidationErrorKind;
use zup_artifact::preset::PresetPackageView;
use zup_core::{
    Installer, NonEmptyString, PresetAsset, PresetRuntime, ProjectPath, ResolvedAsset,
    TargetTriple, Ui,
};
use zup_preset_protocol::{Capabilities, HostOffers};

pub const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;

const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

const ASSET_MARKER: &str = "x-zup-asset";

const SETTINGS_ROOT: &str = "ui.settings";

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
    #[error("{reason}")]
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingProblem {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub package: PathBuf,
    pub runtime: PresetRuntime,
    pub assets: Vec<ResolvedAsset>,
    pub executable: Vec<u8>,
    pub settings_schema: serde_json::Value,
}

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

#[derive(Debug)]
pub struct Selected {
    pub name: String,
    pub version: semver::Version,
    pub protocol: u32,
    pub required_capabilities: Capabilities,
    pub settings: serde_json::Value,
    pub settings_schema: serde_json::Value,
    pub asset_settings: Vec<String>,
    pub executable: Vec<u8>,
}

#[derive(Debug)]
pub struct Prepared {
    pub runtime: PresetRuntime,
    pub assets: Vec<ResolvedAsset>,
    pub executable: Vec<u8>,
}

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

pub fn host_capabilities(installer: &Installer) -> Capabilities {
    zup_artifact::preset::offers(installer)
}

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

fn local_definition(root: &serde_json::Value, reference: &str) -> Option<serde_json::Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    root.get("$defs")?.get(name).cloned()
}

const MAX_REF_DEPTH: u8 = 16;

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
