//! `zup build`: from a project to the files a user downloads.
//!
//! Two shapes of output, one code path. A run that names a `--target` builds that
//! target's own installer. A run that names `--artifact`, or `--universal`, or
//! nothing at all, composes the artifacts the project declares. Everything either
//! shape needs — the payload, the plugins, the runtime template for each target,
//! the release description — is prepared once and shared.
//!
//! The runtime template is not the developer's problem. `zup build` asks the
//! toolchain resolver for the template each target needs, and a contributor
//! working inside this repository stages a local toolchain with one command.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use zup_core::ResolvedTargetConfig;

use crate::artifacts::ArtifactProfile;
use crate::build_inputs::{self, Overwrite};
use crate::cli::BuildCommand;
use crate::project::{self, LoadedProject};
use crate::toolchain::{self, ToolchainResolver};

/// Build the configured distribution artifacts.
pub fn run(args: BuildCommand, toolchain_root: Option<PathBuf>) -> miette::Result<()> {
    let loaded = project::load_for_build(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
    )?;
    let resolver = crate::resolver(toolchain_root);

    // `--target` names a native variant, `--artifact` names a file a user
    // downloads, and they are different questions. A run that names a target
    // builds that target's own installer; a run that names artifacts composes
    // them; a run that names neither builds the project's declared artifacts, or
    // one installer per target when it declares none.
    let intent = build_intent(&args, &loaded.manifest)?;
    let interactive = std::io::stdout().is_terminal();
    match intent {
        Intent::Variants => build_variants(&args, &loaded, &resolver, interactive),
        Intent::Artifacts(profiles) => {
            build_artifacts(&args, &loaded, &profiles, &resolver, interactive)
        }
    }
}

/// What a build run was asked to produce.
enum Intent {
    /// One installer per selected target.
    Variants,
    /// Composed artifacts, in the order they were requested.
    Artifacts(Vec<ArtifactProfile>),
}

/// The runtime template each selected target contributes, in selection order.
///
/// The resolver has already checked every component against its descriptor, so
/// what is left here is which file goes with which target profile. Outputs are
/// not resolved here: a composed artifact has one output per *artifact*, and a
/// per-target output alignment would be the wrong rule for one.
fn resolve_runtimes(
    args: &BuildCommand,
    loaded: &LoadedProject,
    resolver: &ToolchainResolver,
) -> miette::Result<Vec<PathBuf>> {
    let slots = build_inputs::resolve_runtimes(
        build_inputs::InputMode::Enforce,
        resolver,
        &args.runtime,
        &loaded.selected_targets,
    )?;
    Ok(slots
        .iter()
        .map(|slot| {
            slot.path
                .clone()
                .expect("enforced resolution leaves no unresolved slot")
        })
        .collect())
}

/// The dispatcher one composed artifact is built into.
fn resolve_dispatcher(
    args: &BuildCommand,
    profile: &ArtifactProfile,
    composed: &[&zup_artifact::DistributionVariant],
    output: &Path,
    resolver: &ToolchainResolver,
) -> miette::Result<PathBuf> {
    let component = toolchain::dispatcher_for(profile.subsystem(composed), profile.needs_online());
    resolver
        .resolve(
            &component,
            args.dispatcher.first().map(|path| path.as_path()),
        )
        .map_or_else(
            |error| {
                Err(miette::miette!(
                    "`{}` cannot be composed: {}",
                    output.display(),
                    toolchain::missing_component_message(&component, &error)
                ))
            },
            |resolved| Ok(resolved.path),
        )
}

/// Build one self-contained installer per selected target.
fn build_variants(
    args: &BuildCommand,
    loaded: &LoadedProject,
    resolver: &ToolchainResolver,
    interactive: bool,
) -> miette::Result<()> {
    let targets = &loaded.selected_targets;
    let runtimes = build_inputs::resolve_runtimes(
        build_inputs::InputMode::Enforce,
        resolver,
        &args.runtime,
        targets,
    )?;
    let outputs = build_inputs::resolve_outputs(
        build_inputs::InputMode::Enforce,
        Overwrite::from(args.force),
        &args.output,
        &loaded.manifest_path,
        &loaded.manifest.app,
        targets,
        &runtimes,
    )?;
    let runtimes = runtimes
        .iter()
        .map(|slot| {
            slot.path
                .clone()
                .expect("enforced resolution leaves no unresolved slot")
        })
        .collect::<Vec<_>>();
    let outputs = outputs
        .iter()
        .map(|slot| slot.path.clone())
        .collect::<Vec<_>>();

    if interactive {
        println!("→ Validating manifest");
        println!("→ Materializing payload");
    }
    let mut prepared = Vec::with_capacity(targets.len());
    for ((config, target_plan), runtime) in targets.iter().zip(&loaded.build.targets).zip(&runtimes)
    {
        let plugin_artifacts = zup_plugin_build::compile_plugins(target_plan).map_err(|error| {
            miette::miette!("plugin compilation for `{}`: {error}", config.profile)
        })?;
        prepared.push((target_plan, config, runtime, plugin_artifacts));
    }
    if interactive {
        println!("→ Compiling plugins");
    }

    let mut release = crate::artifacts::release_manifest(&loaded.manifest.app);
    for ((target_plan, config, runtime, plugin_artifacts), output) in prepared.iter().zip(&outputs)
    {
        let relative = release_relative(&outputs, output)?;
        if interactive {
            println!("→ Compressing and embedding {}", config.profile);
        }
        let size = project::write_staged(output, args.force, |written| {
            let (size, _) = zup_windows::build_self_contained_executable(
                runtime,
                written,
                target_plan,
                plugin_artifacts,
            )
            .map_err(|error| miette::miette!("installer output: {error}"))?;
            Ok(size)
        })?;
        let payload_bytes: u64 = target_plan.files.iter().map(|file| file.size).sum();
        report_single(target_plan, config, output, size, plugin_artifacts.len());
        release
            .add_artifact(
                &single_target_index(config, target_plan, output),
                &relative,
                zup_artifact::Measured::single(digest_of(output)?, size, payload_bytes),
            )
            .map_err(|error| miette::miette!("release description: {error}"))?;
    }
    finish(args, loaded, &outputs, release, interactive)
}

fn report_single(
    target_plan: &zup_build::TargetBuildPlan,
    config: &ResolvedTargetConfig,
    output: &Path,
    size: u64,
    plugins: usize,
) {
    let payload_bytes: u64 = target_plan.files.iter().map(|file| file.size).sum();
    let updates = target_plan
        .installer
        .updates
        .as_ref()
        .map(|updates| updates.channel.as_str())
        .unwrap_or("not configured");
    println!(
        "Built {} {} ({})",
        target_plan.installer.app.name, target_plan.installer.app.version, config.profile
    );
    println!("  Frontend    {}", config.frontend);
    println!("  Installer   {}", output.display());
    println!("  Size        {}", zup_presentation::format_bytes(size));
    println!("  Target      {}", config.target);
    println!(
        "  Payload     {} files · {}",
        target_plan.files.len(),
        zup_presentation::format_bytes(payload_bytes)
    );
    println!("  Plugins     {plugins}");
    println!("  Updates     {updates}");
}

fn digest_of(path: &Path) -> miette::Result<zup_core::Sha256Digest> {
    project::digest_of(path)
}

/// The index a single-target installer reports, which is a one-variant offline
/// artifact: the same model, described without a dispatcher.
fn single_target_index(
    config: &ResolvedTargetConfig,
    plan: &zup_build::TargetBuildPlan,
    output: &Path,
) -> zup_artifact::ArtifactIndex {
    zup_artifact::ArtifactIndex {
        schema: zup_artifact::ARTIFACT_SCHEMA,
        required_features: zup_artifact::FEATURE_SHARED_CAS,
        media_type: zup_artifact::MediaType::Index,
        artifact: zup_artifact::ArtifactDescriptor {
            id: config.profile.to_string(),
            kind: zup_artifact::ArtifactKind::Single,
            mode: zup_artifact::ArtifactMode::Offline,
            pin: zup_artifact::ArtifactPin::Pinned {
                version: plan.installer.app.version.clone(),
            },
            application: plan.installer.app.clone(),
            launcher: zup_artifact::LauncherStrategy::HostSelectedContainer,
            subsystem: crate::artifacts::subsystem_of(config.frontend),
            output: output_name(output),
            trust: None,
        },
        tables: zup_artifact::ArtifactTables {
            blobs: zup_artifact::Descriptor {
                media_type: zup_artifact::MediaType::BLOB_TABLE,
                digest: zup_artifact::Descriptor::of(zup_artifact::MediaType::BLOB_TABLE, &[])
                    .digest,
                size: 0,
            },
        },
        variants: vec![zup_artifact::VariantDescriptor {
            id: config.profile.to_string(),
            target: config.target.clone(),
            platform: zup_artifact::Platform::from_triple(&config.target),
            frontend: config.frontend,
            manifest: zup_artifact::Descriptor {
                media_type: zup_artifact::MediaType::VARIANT_MANIFEST,
                digest: zup_artifact::Descriptor::of(
                    zup_artifact::MediaType::VARIANT_MANIFEST,
                    &[],
                )
                .digest,
                size: 0,
            },
            requirements: zup_artifact::VariantRequirements::default(),
            runtime: None,
            content: zup_artifact::VariantDescriptorContent {
                logical_size: plan.total_size,
                blob_count: plan.files.len() as u64,
                unique_blob_count: plan.files.len() as u64,
                file_count: plan.files.len() as u64,
                prerequisite_count: plan.prerequisites.len() as u64,
                plugin_count: plan.plugins.len() as u64,
            },
            logical_size: plan.total_size,
        }],
    }
}

/// Build the composed artifacts a run asked for.
fn build_artifacts(
    args: &BuildCommand,
    loaded: &LoadedProject,
    profiles: &[ArtifactProfile],
    resolver: &ToolchainResolver,
    interactive: bool,
) -> miette::Result<()> {
    let app = &loaded.manifest.app;
    let runtimes = resolve_runtimes(args, loaded, resolver)?;
    if args.dispatcher.len() > 1 {
        return Err(miette::miette!(
            "received {} dispatchers; every artifact in one build is composed into the same launcher",
            args.dispatcher.len()
        ));
    }
    let mut variants = Vec::with_capacity(loaded.selected_targets.len());
    for (index, (config, plan)) in loaded
        .selected_targets
        .iter()
        .zip(&loaded.build.targets)
        .enumerate()
    {
        variants.push(resolve_variant(config, plan, &runtimes[index])?);
    }
    if interactive {
        println!("→ Validating manifest");
        println!("→ Materializing payload");
    }

    let mut variant_index = std::collections::BTreeMap::new();
    for (index, config) in loaded.selected_targets.iter().enumerate() {
        variant_index.insert(config.profile.to_string(), index);
    }
    let mut jobs = Vec::with_capacity(profiles.len());
    for (index, profile) in profiles.iter().enumerate() {
        let id = declared_id(args, index, profile);
        let selected: Vec<String> = if profile.targets.is_empty() {
            loaded
                .selected_targets
                .iter()
                .map(|config| config.profile.to_string())
                .collect()
        } else {
            profile
                .targets
                .iter()
                .map(|target| target.to_string())
                .collect()
        };
        let mut indices = Vec::with_capacity(selected.len());
        for name in &selected {
            let found = variant_index.get(name).copied().ok_or_else(|| {
                miette::miette!(
                    "artifact `{id}` includes `{name}`, which is not among the selected targets"
                )
            })?;
            indices.push(found);
        }
        if profile.kind == zup_artifact::ArtifactKind::Single && indices.len() > 1 {
            return Err(miette::miette!(
                "artifact `{id}` is a single-target artifact but includes {} targets",
                indices.len()
            ));
        }
        let file_name = profile.file_name(app.name.as_str(), &app.version);
        jobs.push((id, profile, indices, file_name));
    }

    let named = jobs
        .iter()
        .map(|(id, _, _, file_name)| (id.clone(), file_name.clone()))
        .collect::<Vec<_>>();
    let outputs = crate::artifacts::resolve_outputs(&args.output, &named, &loaded.manifest_path)?;

    let mut release = crate::artifacts::release_manifest(app);
    for ((id, profile, indices, _), output) in jobs.iter().zip(&outputs) {
        let relative = release_relative(&outputs, output)?;
        let composed: Vec<&zup_artifact::DistributionVariant> =
            indices.iter().map(|index| &variants[*index]).collect();
        if interactive {
            println!("→ Composing artifact {id}");
        }
        let request = profile.request(id, output_name(output), app);
        let graph = crate::artifacts::compose(request, &composed)
            .map_err(|error| miette::miette!("artifact `{id}`: {error}"))?;
        let dispatcher = resolve_dispatcher(args, profile, &composed, output, resolver)?;
        let size = project::write_staged(output, args.force, |written| {
            zup_windows::compose_universal_executable(&dispatcher, written, &graph)
                .map_err(|error| miette::miette!("artifact `{id}`: {error}"))?;
            std::fs::metadata(written)
                .map(|meta| meta.len())
                .map_err(|error| miette::miette!("artifact output: {error}"))
        })?;
        let savings = graph.savings();
        release
            .add_artifact(
                graph.index(),
                &relative,
                zup_artifact::Measured::composed(
                    digest_of(output)?,
                    size,
                    graph.table().stored_size(),
                    graph.table().logical_size(),
                    savings.unique_blob_count,
                    savings.standalone_size,
                    savings.shared_size,
                ),
            )
            .map_err(|error| miette::miette!("release description: {error}"))?;
        report_artifact(
            app,
            &ComposedArtifact {
                id,
                profile,
                output,
                size,
                composed: &composed,
                graph: &graph,
                savings: &savings,
            },
        );
    }
    finish(args, loaded, &outputs, release, interactive)
}

/// One resolved distribution variant: a target with its runtime template.
fn resolve_variant(
    config: &ResolvedTargetConfig,
    plan: &zup_build::TargetBuildPlan,
    runtime: &Path,
) -> miette::Result<zup_artifact::DistributionVariant> {
    let bytes = std::fs::read(runtime)
        .map_err(|error| miette::miette!("runtime template `{}`: {error}", runtime.display()))?;
    let media = (zup_artifact::MediaType::RUNTIME, bytes);
    zup_artifact::DistributionVariant::resolve(config, plan, &[], Some(media))
        .map_err(|error| miette::miette!("variant `{}`: {error}", config.profile))
}

/// One composed artifact, as the report describes it.
///
/// The row a build prints is a fact about one artifact, so it is carried as one
/// value rather than as seven parameters that have to be kept in the right order
/// at the call site.
struct ComposedArtifact<'a> {
    id: &'a str,
    profile: &'a ArtifactProfile,
    output: &'a Path,
    size: u64,
    composed: &'a [&'a zup_artifact::DistributionVariant],
    graph: &'a zup_artifact::ArtifactGraph,
    savings: &'a zup_artifact::ArtifactSavings,
}

fn report_artifact(app: &zup_core::App, artifact: &ComposedArtifact<'_>) {
    println!("Built {} {} ({})", app.name, app.version, artifact.id);
    println!(
        "  Kind        {}",
        capitalize(artifact.profile.kind.as_str())
    );
    println!(
        "  Mode        {}",
        capitalize(artifact.profile.mode.as_str())
    );
    println!("  Installer   {}", artifact.output.display());
    println!(
        "  Size        {}",
        zup_presentation::format_bytes(artifact.size)
    );
    println!("  Variants    {}", subsystem_text(artifact.composed));
    println!(
        "  Content     {} unique blobs · {} stored · {} logical",
        artifact.savings.unique_blob_count,
        zup_presentation::format_bytes(artifact.graph.table().stored_size()),
        zup_presentation::format_bytes(artifact.savings.standalone_size),
    );
    println!(
        "  Shared      {} of {} ({:.0}%)",
        zup_presentation::format_bytes(artifact.savings.shared_size),
        zup_presentation::format_bytes(artifact.savings.standalone_size),
        percent(
            artifact.savings.shared_size,
            artifact.savings.standalone_size
        ),
    );
}

fn subsystem_text(composed: &[&zup_artifact::DistributionVariant]) -> String {
    composed
        .iter()
        .map(|variant| format!("{} {}", variant.target(), variant.frontend()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64) * 100.0
    }
}

/// Start a lowercase vocabulary word with a capital, for a summary line that
/// reads as a sentence rather than as a serialized value.
fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// Decide what a build run produces.
fn build_intent(args: &BuildCommand, manifest: &zup_manifest::Manifest) -> miette::Result<Intent> {
    if !args.artifact.is_empty() {
        let mut profiles = Vec::with_capacity(args.artifact.len());
        for id in &args.artifact {
            let profile = manifest.build.artifacts.get(id.as_str()).ok_or_else(|| {
                let available: Vec<&str> = manifest
                    .build
                    .artifacts
                    .keys()
                    .map(|key| key.as_str())
                    .collect();
                miette::miette!(
                    "unknown artifact `{id}`; declared artifacts are {}",
                    if available.is_empty() {
                        "none".to_owned()
                    } else {
                        available.join(", ")
                    }
                )
            })?;
            profiles.push(ArtifactProfile::from_declared(profile));
        }
        return Ok(Intent::Artifacts(profiles));
    }
    if args.universal {
        return Ok(Intent::Artifacts(vec![ArtifactProfile::universal()]));
    }
    if !args.project.target.is_empty() || manifest.build.artifacts.is_empty() {
        return Ok(Intent::Variants);
    }
    Ok(Intent::Artifacts(
        manifest
            .build
            .artifacts
            .values()
            .map(ArtifactProfile::from_declared)
            .collect(),
    ))
}

fn declared_id(args: &BuildCommand, index: usize, profile: &ArtifactProfile) -> String {
    args.artifact
        .get(index)
        .cloned()
        .unwrap_or_else(|| profile.file_name("artifact", &semver::Version::new(0, 0, 0)))
}

fn output_name(output: &Path) -> String {
    output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Setup.exe".to_owned())
}

/// A release has one root: the directory its artifacts are written beside. Paths
/// in the release description are therefore file names, and an output that does
/// not share a directory with the others is refused rather than described with a
/// build-machine path.
fn release_relative(outputs: &[PathBuf], output: &Path) -> miette::Result<String> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    for other in outputs {
        let other_parent = other.parent().unwrap_or_else(|| Path::new("."));
        if other_parent != parent {
            return Err(miette::miette!(
                "a release description describes one directory, but `{}` and `{}` are in different ones",
                output.display(),
                other.display()
            ));
        }
    }
    Ok(output_name(output))
}

/// The directory a build's artifacts are written beside, which is where its
/// release description belongs.
fn release_root(loaded: &LoadedProject, outputs: &[PathBuf]) -> miette::Result<PathBuf> {
    match outputs.split_first() {
        Some((first, rest)) => {
            let parent = first.parent().unwrap_or_else(|| Path::new("."));
            if rest.iter().any(|other| other.parent() != Some(parent)) {
                return Err(miette::miette!(
                    "a release description describes one directory; pass --release-manifest none to skip it"
                ));
            }
            Ok(parent.to_path_buf())
        }
        None => Ok(loaded
            .manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()),
    }
}

/// Write the release description, and say the build is ready to sign.
fn finish(
    args: &BuildCommand,
    loaded: &LoadedProject,
    outputs: &[PathBuf],
    release: zup_artifact::ReleaseManifest,
    interactive: bool,
) -> miette::Result<()> {
    if let Some(destination) = args
        .release_manifest
        .as_deref()
        .filter(|destination| *destination != "none")
    {
        // The description lives in the release root, which is the directory its
        // artifacts are written beside.
        let root = release_root(loaded, outputs)?;
        let path = root.join(destination);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| miette::miette!("release description: {error}"))?;
        }
        let bytes = release
            .encode()
            .map_err(|error| miette::miette!("release description: {error}"))?;
        std::fs::write(&path, bytes)
            .map_err(|error| miette::miette!("release description: {error}"))?;
        if interactive {
            println!("→ Wrote {}", path.display());
        }
    }
    if interactive {
        println!("\n✓ Ready to sign");
    }
    Ok(())
}
