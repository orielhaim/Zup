use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_toolchain::ToolchainComponent;

use crate::toolchain::ToolchainResolver;
use zup_core::{ResolvedTargetConfig, Sha256Digest, Source, TargetOverrides};
use zup_platform::SourceFilePolicy;

#[cfg(windows)]
static WINDOWS_SOURCE_POLICY: zup_windows::WindowsSourceFilePolicy =
    zup_windows::WindowsSourceFilePolicy;
#[cfg(target_os = "linux")]
static LINUX_SOURCE_POLICY: zup_linux::LinuxSourceFilePolicy = zup_linux::LinuxSourceFilePolicy;
static PORTABLE_SOURCE_POLICY: zup_platform::PortableSourceFilePolicy =
    zup_platform::PortableSourceFilePolicy;

pub fn source_policy_for(target: &zup_core::TargetTriple) -> &'static dyn SourceFilePolicy {
    match target.operating_system() {
        zup_core::TargetOperatingSystem::Linux => {
            #[cfg(target_os = "linux")]
            {
                &LINUX_SOURCE_POLICY
            }
            #[cfg(not(target_os = "linux"))]
            {
                &PORTABLE_SOURCE_POLICY
            }
        }
        _ => {
            #[cfg(windows)]
            {
                &WINDOWS_SOURCE_POLICY
            }
            #[cfg(not(windows))]
            {
                &PORTABLE_SOURCE_POLICY
            }
        }
    }
}

pub fn source_policy_for_selection(
    targets: &[ResolvedTargetConfig],
) -> &'static dyn SourceFilePolicy {
    let all_linux = targets
        .iter()
        .all(|target| target.target.operating_system() == zup_core::TargetOperatingSystem::Linux);
    if all_linux && !targets.is_empty() {
        return source_policy_for(&targets[0].target);
    }
    #[cfg(windows)]
    {
        &WINDOWS_SOURCE_POLICY
    }
    #[cfg(not(windows))]
    {
        &PORTABLE_SOURCE_POLICY
    }
}

use zup_core::Frontend;

use crate::cli::ProjectSelection;

#[derive(Debug)]
pub struct SelectedProject {
    pub manifest_path: PathBuf,
    pub manifest_name: String,
    pub source: String,
    pub manifest: zup_manifest::Manifest,
    pub overrides: zup_manifest::TargetOverrideSet,
    pub selected_targets: Vec<ResolvedTargetConfig>,
}

#[derive(Debug)]
pub struct LoadedProject {
    pub manifest_path: PathBuf,
    pub manifest: zup_manifest::Manifest,
    pub selected_targets: Vec<ResolvedTargetConfig>,
    pub build: zup_build::BuildPlan,
    pub presets: Vec<Option<Vec<u8>>>,
}

#[derive(Debug, Clone, Default)]
pub struct TargetOverrideArgs {
    pub source: Vec<PathBuf>,
    pub install_directory: Vec<PathBuf>,
    pub frontend: Option<zup_core::Frontend>,
}

impl TargetOverrideArgs {
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

impl From<&ProjectSelection> for TargetOverrideArgs {
    fn from(value: &ProjectSelection) -> Self {
        Self {
            source: value.source.clone(),
            install_directory: value.install_directory.clone(),
            frontend: value.frontend.map(Frontend::from),
        }
    }
}

pub fn own_package(root: &Path) -> miette::Result<cargo_metadata::Package> {
    let mut command = cargo_metadata::MetadataCommand::new();
    command.no_deps().current_dir(root);
    let metadata = command
        .exec()
        .map_err(|error| miette::miette!("could not read Cargo metadata: {error}"))?;
    let wanted = std::fs::canonicalize(root)
        .map_err(|error| miette::miette!("`{}`: {error}", root.display()))?;
    metadata
        .packages
        .into_iter()
        .find(|package| {
            package
                .manifest_path
                .parent()
                .and_then(|directory| std::fs::canonicalize(directory).ok())
                .is_some_and(|directory| directory == wanted)
        })
        .ok_or_else(|| {
            miette::miette!(
                "`{}` holds no Cargo package; a preset and a plugin are projects of their own",
                root.display()
            )
        })
}

pub fn target_of_kind<'a>(
    package: &'a cargo_metadata::Package,
    kind: &str,
) -> miette::Result<&'a cargo_metadata::Target> {
    package
        .targets
        .iter()
        .find(|target| target.kind.iter().any(|reported| reported.to_string() == kind))
        .ok_or_else(|| {
            miette::miette!(
                "the package `{}` builds no {kind}; its manifest has to say `crate-type = [\"cdylib\"]` \
                 for a plugin, and a preset is a binary",
                package.name
            )
        })
}

pub fn cargo_executable() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

pub fn install_directory_template(path: &Path) -> miette::Result<zup_core::Template> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(miette::miette!(
            "install directory must not contain template variables: {value}"
        ));
    }
    zup_core::Template::parse(&value).map_err(|error| miette::miette!("install directory: {error}"))
}

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
    let policy = source_policy_for_selection(&selected_targets);
    for config in &selected_targets {
        // A Linux GUI target never reaches preset resolution: there is no Linux
        if config.target.operating_system() == zup_core::TargetOperatingSystem::Linux
            && config.frontend == Frontend::Gui
        {
            let errors =
                crate::linux_support::linux_selection_errors(&config.target, config.frontend);
            return Err(miette::miette!("{}", errors.join("\n")));
        }
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
            let resolved = zup_preset_compose::resolve(
                &manifest.ui,
                &project_root,
                &installer,
                &config.target,
                &shipped,
                policy,
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
        policy,
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

fn preset_problem(error: zup_preset_compose::PresetProblem) -> miette::Report {
    let code = match error {
        zup_preset_compose::PresetProblem::Unavailable { .. } => "zup.build.preset_unavailable",
        _ => "zup.build.preset_unusable",
    };
    crate::failure::error(code, error.to_string())
}

pub fn load_manifest(path: &Path) -> miette::Result<zup_manifest::Manifest> {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let source = std::fs::read_to_string(&absolute)
        .map_err(|error| miette::miette!("`{}`: {error}", absolute.display()))?;
    zup_manifest::parse_named(&source, &crate::plain_path(&absolute)).map_err(miette::Report::new)
}

pub fn load_for_build(
    path: &Path,
    selectors: &[String],
    args: &TargetOverrideArgs,
    resolver: &ToolchainResolver,
    writes: zup_build::Writes,
) -> miette::Result<LoadedProject> {
    let selected = select_project(path, selectors, args, false)?;
    for config in &selected.selected_targets {
        crate::build_inputs::check_backend_support(config)?;
        if config.target.operating_system() == zup_core::TargetOperatingSystem::Linux {
            let errors =
                crate::linux_support::linux_selection_errors(&config.target, config.frontend);
            if !errors.is_empty() {
                return Err(miette::miette!("{}", errors.join("\n")));
            }
        }
    }
    let loaded = materialize_project(selected, resolver, writes)?;
    for config in &loaded.selected_targets {
        crate::build_inputs::check_target_lowering(&loaded.build, config)?;
    }
    Ok(loaded)
}

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

pub fn digest_of(path: &Path) -> miette::Result<Sha256Digest> {
    let file = std::fs::File::open(path).map_err(|error| miette::miette!("{error}"))?;
    let (_, digest) = zup_core::hash_reader(file).map_err(|error| miette::miette!("{error}"))?;
    Ok(digest)
}

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
