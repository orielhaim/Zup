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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_automation::{AutomationResult, BuildDetails, Details, LogLevel};
use zup_core::ResolvedTargetConfig;

use crate::artifacts::ArtifactProfile;
use crate::build_inputs::{self, Overwrite};
use crate::cli::BuildCommand;
use crate::project::{self, LoadedProject};
use crate::report::Reporter;
use crate::toolchain::{self, ToolchainResolver};

/// What a build produced.
///
/// The domain answer, in domain terms: the release description a consumer verifies
/// against, and the two documents written beside it. `zup build` knows this before it
/// knows anything about JSON, and the adapter that turns it into an
/// [`AutomationResult`] is one function away in `crate::automation` — so a build that
/// fails halfway still says what it managed to write, and a caller never has to go
/// looking in `dist/` for what happened.
pub struct BuildOutcome {
    /// The release description, exactly as it was written.
    pub release: zup_artifact::ReleaseManifest,
    /// Where the release description went, project-relative, or `None` when the build
    /// was told to skip it.
    pub release_manifest: Option<String>,
    /// Where the signing plan went, project-relative.
    pub signing_plan: Option<String>,
    /// How many files the plan says an external signer has to touch.
    pub pending_signatures: usize,
}

/// Build the configured distribution artifacts.
pub fn run(
    args: BuildCommand,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let outcome = execute(&args, &reporter, toolchain_root)?;
    let mut result = crate::automation::release_result(
        zup_automation::OPERATION_BUILD,
        &outcome.release,
        &[], // filled in below from the release's own variants
        outcome.release_manifest.clone(),
    );
    // The targets a build covered are the release's own variants, not the profiles the
    // caller typed: a composed artifact names every variant it carries, and that is the
    // set a consumer has to know about.
    result = result.with_targets(
        outcome
            .release
            .variants
            .iter()
            .map(|variant| {
                zup_automation::Target::new(variant.id.clone(), variant.target.to_string())
            })
            .collect(),
    );
    result = result.with_details(Details::Build(BuildDetails {
        signing_plan: outcome.signing_plan.clone(),
        pending_signatures: outcome.pending_signatures,
    }));
    Ok(result)
}

/// The build itself, with no wire types in it.
fn execute(
    args: &BuildCommand,
    reporter: &Reporter,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<BuildOutcome> {
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
    let intent = build_intent(args, &loaded.manifest)?;
    reporter.phase(
        "validate",
        "→ Validating manifest and resolving the toolchain",
    );
    match intent {
        Intent::Variants => build_variants(args, &loaded, &resolver, reporter),
        Intent::Artifacts(profiles) => {
            build_artifacts(args, &loaded, &profiles, &resolver, reporter)
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
                Err(crate::failure::error_with_help(
                    "zup.toolchain.component_missing",
                    format!(
                        "`{}` cannot be composed: {}",
                        output.display(),
                        toolchain::missing_component_message(&component, &error)
                    ),
                    crate::doctor::TOOLCHAIN_HINT,
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
    reporter: &Reporter,
) -> miette::Result<BuildOutcome> {
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

    reporter.phase("payload", "→ Materializing payload and compiling plugins");
    let mut prepared = Vec::with_capacity(targets.len());
    for ((config, target_plan), runtime) in targets.iter().zip(&loaded.build.targets).zip(&runtimes)
    {
        let plugin_artifacts = zup_plugin_build::compile_plugins(target_plan).map_err(|error| {
            crate::failure::error_with_help(
                "zup.build.plugin_compile_failed",
                format!("plugin compilation for `{}`: {error}", config.profile),
                "Fix the plugin source, or remove it from the profile.",
            )
        })?;
        prepared.push((target_plan, config, runtime, plugin_artifacts));
    }

    let mut release = crate::artifacts::release_manifest(&loaded.manifest.app);
    for ((target_plan, config, runtime, plugin_artifacts), output) in prepared.iter().zip(&outputs)
    {
        let relative = release_relative(&outputs, output)?;
        reporter.phase(
            "compose",
            format!(
                "→ Compressing and embedding {} for {}",
                config.profile, config.target
            ),
        );
        let size = project::write_staged(output, args.force, |written| {
            let (size, _) = zup_windows::build_self_contained_executable(
                runtime,
                written,
                target_plan,
                plugin_artifacts,
            )
            .map_err(|error| {
                crate::failure::error(
                    "zup.build.compose_failed",
                    format!("installer output: {error}"),
                )
            })?;
            Ok(size)
        })?;
        let payload_bytes: u64 = target_plan.files.iter().map(|file| file.size).sum();
        report_single(
            reporter,
            target_plan,
            config,
            output,
            size,
            plugin_artifacts.len(),
        );
        release
            .add_single_target(
                &single_target(
                    config,
                    target_plan,
                    crate::artifacts::subsystem_of(config.frontend),
                ),
                &relative,
                zup_artifact::Measured::single(digest_of(output)?, size, payload_bytes),
            )
            .map_err(|error| {
                crate::failure::error(
                    "zup.build.release_description",
                    format!("release description: {error}"),
                )
            })?;
    }
    finish(args, loaded, &outputs, release, reporter)
}

/// What one per-target build says, for a person.
fn report_single(
    reporter: &Reporter,
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
    reporter.log(
        LogLevel::Info,
        format!(
            "Built {} {} ({})\n  Frontend    {}\n  Installer   {}\n  Size        {}\n  Target      \
             {}\n  Payload     {} files · {}\n  Plugins     {}\n  Updates     {}",
            target_plan.installer.app.name,
            target_plan.installer.app.version,
            config.profile,
            config.frontend,
            output.display(),
            zup_presentation::format_bytes(size),
            config.target,
            target_plan.files.len(),
            zup_presentation::format_bytes(payload_bytes),
            plugins,
            updates,
        ),
    );
}

fn digest_of(path: &Path) -> miette::Result<zup_core::Sha256Digest> {
    project::digest_of(path)
}

/// What a per-target installer reports in the release description.
///
/// Deliberately smaller than the `ArtifactIndex` a composed artifact carries: a
/// per-target build has no graph, no dispatcher, and no shared store, so the only
/// things a reader needs are which machine it is for, what it presents, and how
/// big its plan is.
fn single_target(
    config: &ResolvedTargetConfig,
    plan: &zup_build::TargetBuildPlan,
    subsystem: zup_artifact::LauncherSubsystem,
) -> zup_artifact::SingleTarget {
    zup_artifact::SingleTarget {
        id: config.profile.to_string(),
        version: plan.installer.app.version.clone(),
        target: config.target.clone(),
        platform: zup_artifact::Platform::from_triple(&config.target),
        frontend: config.frontend,
        subsystem,
        file_count: plan.files.len() as u64,
        prerequisite_count: plan.prerequisites.len() as u64,
        plugin_count: plan.plugins.len() as u64,
    }
}

/// Build the composed artifacts a run asked for.
fn build_artifacts(
    args: &BuildCommand,
    loaded: &LoadedProject,
    profiles: &[ArtifactProfile],
    resolver: &ToolchainResolver,
    reporter: &Reporter,
) -> miette::Result<BuildOutcome> {
    let app = &loaded.manifest.app;
    let runtimes = resolve_runtimes(args, loaded, resolver)?;
    if args.dispatcher.len() > 1 {
        return Err(crate::failure::error_with_help(
            "zup.build.multiple_dispatchers",
            format!(
                "received {} dispatchers; every artifact in one build is composed into the same \
                 launcher",
                args.dispatcher.len()
            ),
            "Pass at most one `--dispatcher`.",
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
    reporter.phase("payload", "→ Validating manifest and materializing payload");
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
                crate::failure::error_with_help(
                    "zup.build.unknown_target",
                    format!(
                        "artifact `{id}` includes `{name}`, which is not among the selected targets"
                    ),
                    format!("Add `--target {name}`, or remove it from the artifact's `targets`."),
                )
            })?;
            indices.push(found);
        }
        if profile.kind == zup_artifact::ArtifactKind::Single && indices.len() > 1 {
            return Err(crate::failure::error_with_help(
                "zup.build.single_target_artifact",
                format!(
                    "artifact `{id}` is a single-target artifact but includes {} targets",
                    indices.len()
                ),
                "Split it into one artifact per target, or make it universal.",
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
        reporter.phase("compose", format!("→ Composing artifact {id}"));
        let composed: Vec<&zup_artifact::DistributionVariant> =
            indices.iter().map(|index| &variants[*index]).collect();
        let request = profile.request(id, output_name(output), app);
        let graph = crate::artifacts::compose(request, &composed).map_err(|error| {
            crate::failure::error(
                "zup.build.compose_failed",
                format!("artifact `{id}`: {error}"),
            )
        })?;
        let dispatcher = resolve_dispatcher(args, profile, &composed, output, resolver)?;
        let size = project::write_staged(output, args.force, |written| {
            zup_windows::compose_universal_executable(&dispatcher, written, &graph).map_err(
                |error| {
                    crate::failure::error(
                        "zup.build.compose_failed",
                        format!("artifact `{id}`: {error}"),
                    )
                },
            )?;
            std::fs::metadata(written)
                .map(|meta| meta.len())
                .map_err(|error| {
                    crate::failure::error(
                        "zup.build.output_unreadable",
                        format!("artifact output: {error}"),
                    )
                })
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
            .map_err(|error| {
                crate::failure::error(
                    "zup.build.release_description",
                    format!("release description: {error}"),
                )
            })?;
        report_artifact(
            reporter,
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
    finish(args, loaded, &outputs, release, reporter)
}

/// One resolved distribution variant: a target with its runtime template.
fn resolve_variant(
    config: &ResolvedTargetConfig,
    plan: &zup_build::TargetBuildPlan,
    runtime: &Path,
) -> miette::Result<zup_artifact::DistributionVariant> {
    let bytes = std::fs::read(runtime).map_err(|error| {
        crate::failure::error(
            "zup.build.runtime_unreadable",
            format!("runtime template `{}`: {error}", runtime.display()),
        )
    })?;
    let media = (zup_artifact::MediaType::RUNTIME, bytes);
    zup_artifact::DistributionVariant::resolve(config, plan, &[], Some(media)).map_err(|error| {
        crate::failure::error(
            "zup.build.variant_invalid",
            format!("variant `{}`: {error}", config.profile),
        )
    })
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

fn report_artifact(reporter: &Reporter, app: &zup_core::App, artifact: &ComposedArtifact<'_>) {
    reporter.log(
        LogLevel::Info,
        format!(
            "Built {} {} ({})\n  Kind        {}\n  Mode        {}\n  Installer   {}\n  Size        {}\n  \
             Variants    {}\n  Content     {} unique blobs · {} stored · {} logical\n  Shared      {} of \
             {} ({:.0}%)\n  Digest      sha256:{}",
            app.name,
            app.version,
            artifact.id,
            capitalize(artifact.profile.kind.as_str()),
            capitalize(artifact.profile.mode.as_str()),
            artifact.output.display(),
            zup_presentation::format_bytes(artifact.size),
            subsystem_text(artifact.composed),
            artifact.savings.unique_blob_count,
            zup_presentation::format_bytes(artifact.graph.table().stored_size()),
            zup_presentation::format_bytes(artifact.savings.standalone_size),
            zup_presentation::format_bytes(artifact.savings.shared_size),
            zup_presentation::format_bytes(artifact.savings.standalone_size),
            percent(
                artifact.savings.shared_size,
                artifact.savings.standalone_size
            ),
            crate::project::digest_of(artifact.output)
                .map(|digest| digest.to_hex())
                .unwrap_or_else(|_| "unreadable".to_owned()),
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
                crate::failure::error_with_help(
                    "zup.build.unknown_artifact",
                    format!(
                        "unknown artifact `{id}`; declared artifacts are {}",
                        if available.is_empty() {
                            "none".to_owned()
                        } else {
                            available.join(", ")
                        }
                    ),
                    "Check the spelling against `[build.artifacts]` in the manifest.",
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
            return Err(crate::failure::error_with_help(
                "zup.build.split_release",
                format!(
                    "a release description describes one directory, but `{}` and `{}` are in \
                     different ones",
                    output.display(),
                    other.display()
                ),
                "Give every `--output` the same directory, or pass `--release-manifest none`.",
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
                return Err(crate::failure::error_with_help(
                    "zup.build.split_release",
                    "a release description describes one directory; pass --release-manifest none to skip it",
                    "Give every `--output` the same directory.",
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

/// Write the release description and the signing plan, and say what remains.
///
/// Two documents, because they answer two different questions and are read at
/// two different times. `zup-release.json` says what the build produced and is
/// what a publisher reads; `zup-signing.json` says what an external signer has to
/// touch, in what order, and is what a signing step reads. Collapsing them would
/// mean the release description carries a credential-free signing instruction
/// that a build cannot act on, or that a publisher is free to ignore.
///
/// Neither is written with a bare `write`: both are documents a later step parses
/// and trusts, so they are published atomically and flushed, exactly like the
/// ledger a running installer relies on.
fn finish(
    args: &BuildCommand,
    loaded: &LoadedProject,
    outputs: &[PathBuf],
    release: zup_artifact::ReleaseManifest,
    reporter: &Reporter,
) -> miette::Result<BuildOutcome> {
    // `none` is the one value that is a name rather than a path, and it is how a
    // project opts out of producing a release it does not intend to publish.
    if args.release_manifest == "none" {
        return Ok(BuildOutcome {
            release,
            release_manifest: None,
            signing_plan: None,
            pending_signatures: 0,
        });
    }
    let destination = args.release_manifest.as_str();
    // The description lives in the release root, which is the directory its
    // artifacts are written beside.
    let root = release_root(loaded, outputs)?;
    let path = root.join(destination);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            crate::failure::error(
                "zup.build.release_unwritable",
                format!("release description: {error}"),
            )
        })?;
    }
    let bytes = release.encode().map_err(|error| {
        crate::failure::error(
            "zup.build.release_unencodable",
            format!("release description: {error}"),
        )
    })?;
    zup_windows::write_durable(&path, &bytes).map_err(|error| {
        crate::failure::error(
            "zup.build.release_unwritable",
            format!("release description: {error}"),
        )
    })?;

    let plan = signing_plan(&release, &root, args.signing_subject.as_deref())?;
    let pending = plan.pre_compose().count() + plan.post_compose().count();
    let plan_path = path.with_file_name(zup_signing::SIGNING_PLAN_NAME);
    zup_windows::write_durable(
        &plan_path,
        &plan.encode().map_err(|error| {
            crate::failure::error(
                "zup.build.signing_plan_unencodable",
                format!("signing plan: {error}"),
            )
        })?,
    )
    .map_err(|error| {
        crate::failure::error(
            "zup.build.signing_plan_unwritable",
            format!("signing plan: {error}"),
        )
    })?;

    reporter.log(
        LogLevel::Info,
        format!(
            "→ Wrote {}\n→ Wrote {}\n\n{} to sign:\n{}\n\nSign them, then run `zup sign verify` to \
             finalize the release.",
            crate::automation::project_path(&path),
            crate::automation::project_path(&plan_path),
            match (plan.pre_compose().count(), plan.post_compose().count()) {
                (0, 0) => "Nothing".to_owned(),
                (0, post) => format!("{post} file(s)"),
                (pre, 0) => format!("{pre} file(s)"),
                (pre, post) => format!("{pre} native runtime(s), then {post} artifact(s)"),
            },
            plan.steps
                .iter()
                .map(|step| format!("  {:<14} {}", step.role.as_str(), step.subject.path))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    );
    Ok(BuildOutcome {
        release,
        release_manifest: Some(crate::automation::project_path(&path)),
        signing_plan: Some(crate::automation::project_path(&plan_path)),
        pending_signatures: pending,
    })
}

/// Derive the signing plan from what the build composed.
///
/// The plan is *derived* rather than accumulated, so it cannot disagree with the
/// release description it is written beside. The interesting part is which files
/// land in the pre-compose set: a single-target installer is its own runtime, so
/// it is one post-compose file; a composed artifact embeds a runtime that is
/// extracted and executed separately, so the runtime it was composed from is a
/// pre-compose file of its own.
fn signing_plan(
    release: &zup_artifact::ReleaseManifest,
    root: &Path,
    subject: Option<&str>,
) -> miette::Result<zup_signing::SigningPlan> {
    let mut requirement = zup_signing::SigningRequirement::production();
    if let Some(subject) = subject {
        requirement = requirement.signed_by(subject);
    }
    let mut plan = zup_signing::SigningPlan::new(&release.application, requirement);

    // Pre-compose: every runtime an artifact embeds, named by the file it was
    // composed from. A build does not copy the runtime into the release root — the
    // toolchain owns those bytes and a release pipeline stages the *signed* copy
    // there before composing — so the path recorded is the one verification will
    // look at, and it is written by whatever signed it.
    for (variant, _carriers) in embedded_by_variant(release) {
        let path = format!("runtime/{variant}.exe");
        let file = root.join(&path);
        plan.push(zup_signing::SigningStep::new(
            zup_signing::SigningRole::NativeRuntime,
            zup_signing::SigningStage::PreCompose,
            zup_signing::SigningReason::VariantRuntime {
                variant: variant.clone(),
            },
            zup_signing::SigningSubject {
                path,
                digest: crate::project::digest_of(&file)
                    .unwrap_or(zup_core::Sha256Digest::from_bytes([0; 32])),
                size: std::fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0),
                variants: vec![variant],
            },
        ))
        .map_err(|error| {
            crate::failure::error(
                "zup.build.signing_plan_rejected",
                format!("signing plan: {error}"),
            )
        })?;
    }

    for artifact in &release.artifacts {
        plan.push(zup_signing::SigningStep::new(
            zup_signing::SigningRole::OuterArtifact,
            zup_signing::SigningStage::PostCompose,
            zup_signing::SigningReason::Installer {
                artifact: artifact.id.clone(),
            },
            zup_signing::SigningSubject {
                path: artifact.path.clone(),
                digest: artifact.built.digest,
                size: artifact.built.size,
                variants: artifact.variants.clone(),
            },
        ))
        .map_err(|error| {
            crate::failure::error(
                "zup.build.signing_plan_rejected",
                format!("signing plan: {error}"),
            )
        })?;
    }
    Ok(plan)
}

/// The variants a release's universal artifacts embed, as `runtime/<variant>.exe`.
///
/// Only universal artifacts embed a runtime, because only a universal artifact is
/// built around a dispatcher: the dispatcher is the base image, and each variant's
/// runtime is a resource inside it. A single-target installer *is* its runtime.
fn embedded_by_variant(release: &zup_artifact::ReleaseManifest) -> Vec<(String, Vec<String>)> {
    let mut by_variant: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for artifact in &release.artifacts {
        if artifact.kind != zup_artifact::ArtifactKind::Universal {
            continue;
        }
        for variant in &artifact.variants {
            by_variant
                .entry(variant.clone())
                .or_default()
                .push(artifact.id.clone());
        }
    }
    by_variant.into_iter().collect()
}
