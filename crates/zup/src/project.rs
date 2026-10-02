//! Reading a project: the manifest, its target profiles, and its payload.
//!
//! Selection touches no source tree, so a caller can reject a target this host
//! cannot build before paying for materialization. That is the whole reason
//! selection and materialization are two steps and not one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_toolchain::ToolchainComponent;

use crate::toolchain::ToolchainResolver;
use zup_core::{ResolvedTargetConfig, Sha256Digest, Source, TargetOverrides};

use zup_core::Frontend;

use crate::cli::ProjectSelection;

/// A read manifest and its selected profiles, before anything is materialized.
#[derive(Debug)]
pub struct SelectedProject {
    pub manifest_path: PathBuf,
    pub manifest_name: String,
    pub source: String,
    pub manifest: zup_manifest::Manifest,
    /// The caller's per-profile overrides, in the form `compile` revalidates.
    pub overrides: zup_manifest::TargetOverrideSet,
    pub selected_targets: Vec<ResolvedTargetConfig>,
}

/// A compiled, materialized project: what a build actually reads.
#[derive(Debug)]
pub struct LoadedProject {
    pub manifest_path: PathBuf,
    pub manifest: zup_manifest::Manifest,
    pub selected_targets: Vec<ResolvedTargetConfig>,
    pub build: zup_build::BuildPlan,
    /// The preset executable each selected target will launch, in target order.
    ///
    /// Not part of the plan because it is a native program rather than a file the
    /// project ships: composition writes it, and nothing materializes it from the
    /// source tree. `None` for a target that presents no window, which is why this
    /// is an option rather than an empty vector: an empty preset resource is still
    /// a resource, and one is not what a console installer carries.
    pub presets: Vec<Option<Vec<u8>>>,
}

/// The caller-supplied per-target overrides of an authoring command.
///
/// Every repeatable flag here is aligned against the selected target count, so a
/// single value against several targets is an error rather than a broadcast.
#[derive(Debug, Clone, Default)]
pub struct TargetOverrideArgs {
    pub source: Vec<PathBuf>,
    pub install_directory: Vec<PathBuf>,
    pub frontend: Option<zup_core::Frontend>,
}

impl TargetOverrideArgs {
    /// The per-profile overrides, aligned with the selected profiles in order.
    pub fn resolve(
        &self,
        selected: &[ResolvedTargetConfig],
    ) -> miette::Result<zup_manifest::TargetOverrideSet> {
        let sources =
            crate::build_inputs::align_per_target("sources", "--source", &self.source, selected)?;
        let directories = crate::build_inputs::align_per_target(
            "install directories",
            "--install-directory",
            &self.install_directory,
            selected,
        )?;
        let mut overrides = zup_manifest::TargetOverrideSet::default();
        for (index, config) in selected.iter().enumerate() {
            let source = sources
                .and_then(|sources| sources.get(index))
                .map(|path| Source::new(path.clone()))
                .transpose()
                .map_err(|error| miette::miette!("--source: {error}"))?;
            let install_directory = directories
                .and_then(|directories| directories.get(index))
                .map(|path| install_directory_template(path))
                .transpose()?;
            overrides.apply(
                config.profile.clone(),
                TargetOverrides {
                    source,
                    install_directory,
                    frontend: self.frontend,
                },
            );
        }
        Ok(overrides)
    }
}

/// The per-target overrides a `ProjectSelection` carries.
impl From<&ProjectSelection> for TargetOverrideArgs {
    fn from(value: &ProjectSelection) -> Self {
        Self {
            source: value.source.clone(),
            install_directory: value.install_directory.clone(),
            frontend: value.frontend.map(Frontend::from),
        }
    }
}

/// A directory a caller named, as a path template.
///
/// A template variable in a chosen path is a refusal rather than a literal
/// directory name: `${location.user_data}` in an install directory would create a
/// directory whose name is that string.
pub fn install_directory_template(path: &Path) -> miette::Result<zup_core::Template> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(miette::miette!(
            "install directory must not contain template variables: {value}"
        ));
    }
    zup_core::Template::parse(&value).map_err(|error| miette::miette!("install directory: {error}"))
}

/// Read a manifest and resolve its selected targets without materializing them.
pub fn select_project(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
    single: bool,
) -> miette::Result<SelectedProject> {
    let manifest_path = path
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source = std::fs::read_to_string(&manifest_path)
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let manifest_name = crate::plain_path(&manifest_path);
    let manifest =
        zup_manifest::parse_named(&source, &manifest_name).map_err(miette::Report::new)?;
    let effective = if single && selectors.is_empty() {
        if manifest.build.targets.len() != 1 {
            return Err(miette::miette!(
                "this command requires exactly one target; pass --target when the manifest \
                 declares multiple profiles"
            ));
        }
        vec![
            manifest
                .build
                .targets
                .keys()
                .next()
                .expect("manifest has one target")
                .to_string(),
        ]
    } else {
        selectors.to_vec()
    };
    let selector_refs = effective.iter().map(String::as_str).collect::<Vec<_>>();
    // The un-overridden selection names the profiles and their count, which is
    // what the repeatable flags align against.
    let selection =
        zup_manifest::select_targets(&manifest, &selector_refs, &TargetOverrides::default())
            .map_err(|error| {
                miette::Report::new(error.with_source_named(&source, &manifest_name))
            })?;
    let overrides = args.resolve(&selection)?;
    let selected_targets = zup_manifest::select_targets_with(&manifest, &selector_refs, &overrides)
        .map_err(|error| miette::Report::new(error.with_source_named(&source, &manifest_name)))?;
    if single && selected_targets.len() != 1 {
        return Err(miette::miette!(
            "this command accepts exactly one target; pass one --target"
        ));
    }
    Ok(SelectedProject {
        manifest_path,
        manifest_name,
        source,
        manifest,
        overrides,
        selected_targets,
    })
}

/// Compile and materialize the selected targets of a project.
///
/// A target that presents a window gets its preset selected here, before
/// materialization, because a preset's settings name project files the build has
/// to resolve and the compiled installer has to carry. The selection is one path
/// whether the package came from `[ui].preset` or from the toolchain's own.
pub fn materialize_project(
    selected: SelectedProject,
    resolver: &ToolchainResolver,
    writes: zup_build::Writes,
) -> miette::Result<LoadedProject> {
    let SelectedProject {
        manifest_path,
        manifest_name,
        source,
        manifest,
        overrides,
        selected_targets,
    } = selected;
    let project_root = zup_build::project_root(&manifest_path);
    let mut compiled = Vec::with_capacity(selected_targets.len());
    let mut ui_assets = BTreeMap::new();
    let mut executables = Vec::with_capacity(selected_targets.len());
    for config in &selected_targets {
        let mut installer =
            zup_manifest::compile(&manifest, config, overrides.get(&config.profile)).map_err(
                |error| miette::Report::new(error.with_source_named(&source, &manifest_name)),
            )?;
        if installer.frontend == Frontend::Gui {
            let shipped = || {
                resolver
                    .resolve(&ToolchainComponent::Preset, None)
                    .map(|resolved| resolved.path)
                    .map_err(|error| error.to_string())
            };
            // The one resolver, so a preview and a build cannot disagree about
            // which window this application presents. It is asked for the
            // preset zup ships only when the project named none, because finding
            // it costs a directory walk an application that chose its own window
            // should not pay.
            let resolved = zup_ui_compose::resolve(
                &manifest.ui,
                &project_root,
                &installer,
                &config.target,
                &shipped,
                &zup_windows::WindowsSourceFilePolicy,
            )
            .map_err(preset_problem)?;
            installer.preset = Some(resolved.runtime);
            ui_assets.insert(config.profile.clone(), resolved.assets);
            executables.push(Some(resolved.executable));
        } else {
            executables.push(None);
        }
        compiled.push((config.clone(), installer));
    }

    let build = zup_build::materialize_with_assets(
        &manifest_path,
        &manifest,
        compiled,
        &ui_assets,
        &zup_windows::WindowsSourceFilePolicy,
        writes,
    )
    .map_err(miette::Report::new)?;
    Ok(LoadedProject {
        manifest_path,
        manifest,
        selected_targets,
        build,
        presets: executables,
    })
}

/// Select, compile, and materialize in one step.
pub fn load_single_project(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
    resolver: &ToolchainResolver,
    writes: zup_build::Writes,
) -> miette::Result<LoadedProject> {
    materialize_project(
        select_project(path, selectors, args, true)?,
        resolver,
        writes,
    )
}

/// A window that could not be presented, as the diagnostic a build reports.
///
/// Two codes because there are two mistakes: nothing could be obtained at all, so
/// the machine is missing a component or the project named a file that is not
/// there; or something was obtained and it does not work here. A caller that has
/// to tell those apart - a CI system deciding whether to stage a toolchain - can,
/// and a build that collapsed them would answer that question wrongly.
fn preset_problem(error: zup_ui_compose::PresetProblem) -> miette::Report {
    let code = match error {
        zup_ui_compose::PresetProblem::Unavailable { .. } => "zup.build.preset_unavailable",
        _ => "zup.build.preset_unusable",
    };
    crate::failure::error(code, error.to_string())
}

/// Read and compile a manifest, for a command that does not need its sources.
pub fn load_manifest(path: &Path) -> miette::Result<zup_manifest::Manifest> {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let source = std::fs::read_to_string(&absolute)
        .map_err(|error| miette::miette!("`{}`: {error}", absolute.display()))?;
    zup_manifest::parse_named(&source, &crate::plain_path(&absolute)).map_err(miette::Report::new)
}

/// Read a manifest, refusing when this host cannot build one of its targets.
///
/// A refusal here costs nothing: no source tree is walked, no plugin is compiled,
/// and no prerequisite is resolved. That is the difference between a clear message
/// and a slow one.
pub fn load_for_build(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
    resolver: &ToolchainResolver,
    writes: zup_build::Writes,
) -> miette::Result<LoadedProject> {
    let selected = select_project(path, selectors, args, false)?;
    // The backend boundary reads no files, so an unsupported target is refused
    // before the source tree is walked and prerequisites are resolved.
    for config in &selected.selected_targets {
        crate::build_inputs::check_backend_support(config)?;
    }
    let loaded = materialize_project(selected, resolver, writes)?;
    for config in &loaded.selected_targets {
        crate::build_inputs::check_target_lowering(&loaded.build, config)?;
    }
    Ok(loaded)
}

/// Create the directory an output will be written into, so a caller naming a
/// directory that does not exist yet gets the installer instead of an I/O error.
pub fn ensure_output_parent(output: &Path) -> miette::Result<()> {
    let Some(parent) = output.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent)
        .map_err(|error| miette::miette!("output directory {}: {error}", parent.display()))
}

/// A staging path beside an output, on the same volume so the move into place
/// cannot cross a filesystem boundary.
pub fn staging_output(output: &Path) -> miette::Result<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| miette::miette!("output `{}` has no file name", output.display()))?;
    let staging = output.with_file_name(format!(
        "{}.{}.staging",
        name.to_string_lossy(),
        std::process::id()
    ));
    if staging.exists() {
        std::fs::remove_file(&staging)
            .map_err(|error| miette::miette!("stale staging file: {error}"))?;
    }
    Ok(staging)
}

/// Put a finished artifact where the caller asked for it, replacing whatever
/// `--force` authorized replacing.
pub fn replace_output(staging: &Path, output: &Path) -> miette::Result<()> {
    if output.exists() {
        std::fs::remove_file(output)
            .map_err(|error| miette::miette!("replace `{}`: {error}", output.display()))?;
    }
    std::fs::rename(staging, output).map_err(|error| {
        miette::miette!(
            "move `{}` to `{}`: {error}",
            staging.display(),
            output.display()
        )
    })
}

/// Write an artifact through a staging file when replacing an existing one, and
/// return the writer's own result.
///
/// A partially written installer is worse than no installer: it looks like a
/// build output, and the next build refuses to replace it without `--force`. So
/// the bytes are written beside the destination on the same volume, and the move
/// into place is the last thing that happens.
pub fn write_staged<T>(
    output: &Path,
    force: bool,
    write: impl FnOnce(&Path) -> miette::Result<T>,
) -> miette::Result<T> {
    ensure_output_parent(output)?;
    let staging = if force && output.exists() {
        Some(staging_output(output)?)
    } else {
        None
    };
    let target = staging.as_deref().unwrap_or(output);
    let written = match write(target) {
        Ok(value) => value,
        Err(error) => {
            if let Some(staging) = &staging {
                let _ = std::fs::remove_file(staging);
            }
            return Err(error);
        }
    };
    if let Some(staging) = staging
        && let Err(error) = replace_output(&staging, output)
    {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(written)
}

/// The SHA-256 of a file.
pub fn digest_of(path: &Path) -> miette::Result<Sha256Digest> {
    let file = std::fs::File::open(path).map_err(|error| miette::miette!("{error}"))?;
    let (_, digest) = zup_core::hash_reader(file).map_err(|error| miette::miette!("{error}"))?;
    Ok(digest)
}

/// A TOML basic string, escaped.
///
/// Hand-built so `zup init` writes a manifest that is byte-identical on every
/// platform and readable, rather than one that round-trips through a serializer
/// whose output changes between releases.
pub fn toml_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            character if character.is_control() => {
                use std::fmt::Write;
                let _ = write!(output, "\\u{:04X}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

/// A filesystem-safe slug, for a directory name a manifest template will use.
pub fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "app".into()
    } else {
        output
    }
}

/// The resolved install directory of a target, as one line.
///
/// A single scope's template is shown on its own; both are labeled when the
/// target installs to two scopes.
pub fn install_directory_text(install: &zup_core::Install) -> String {
    let user = install.directory.user.as_ref().map(ToString::to_string);
    let machine = install.directory.machine.as_ref().map(ToString::to_string);
    match (user, machine) {
        (Some(user), None) => user,
        (None, Some(machine)) => machine,
        (None, None) => "no directory template".to_owned(),
        (Some(user), Some(machine)) => format!("user={user} machine={machine}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_value_is_escaped_rather_than_trusted() {
        assert_eq!(toml_string("Acme"), "\"Acme\"");
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(toml_string("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn a_slug_is_filesystem_safe_and_never_empty() {
        assert_eq!(slug("Acme Desktop"), "acme-desktop");
        assert_eq!(slug("  ???  "), "app");
        // A run of characters that is not ASCII alphanumeric becomes one
        // separator, wherever it falls, so a name in any script still reads as
        // words rather than as one run.
        assert_eq!(slug("Ünïcode"), "n-code");
    }

    #[test]
    fn a_chosen_install_directory_is_not_a_template() {
        let error = install_directory_template(Path::new("/opt/${location.user_data}"))
            .expect_err("a template is not a chosen path")
            .to_string();
        assert!(error.contains("template variables"), "{error}");
    }
}
