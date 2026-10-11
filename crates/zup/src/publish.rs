use std::path::{Path, PathBuf};

use zup_automation::{
    Artifact, AutomationResult, ByteCount, Details, LogLevel, Publication, PublishStageDetails,
    StagedPackage,
};
#[cfg(windows)]
use zup_core::InstallScope;

use crate::cli::{PublishGithubCommand, PublishStageCommand};
use crate::failure::Reporter;
use crate::project::{self, LoadedProject};
use crate::toolchain::{self, ToolchainResolver};

pub fn run_stage(
    args: PublishStageCommand,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let selection = crate::cli::ProjectSelection {
        manifest: args.manifest.clone(),
        target: args.target.clone(),
        source: args.source.clone(),
        install_directory: args.install_directory.clone(),
        frontend: args.frontend,
    };
    let loaded = project::load_for_build(
        &selection.manifest,
        &selection.target,
        &selection.overrides(),
        &crate::resolver(toolchain_root.clone())?,
        zup_build::Writes::Publish,
    )?;
    let resolver = crate::resolver(toolchain_root)?;
    let runtimes = resolve_runtimes(&loaded, &resolver)?;

    let thin = args.thin;
    let mut variants = Vec::with_capacity(loaded.selected_targets.len());
    for (index, (config, plan)) in loaded
        .selected_targets
        .iter()
        .zip(&loaded.build.targets)
        .enumerate()
    {
        let mut natives = if thin {
            vec![(
                zup_artifact::MediaType::RUNTIME,
                plan_only_runtime(&runtimes[index], plan, config)?,
            )]
        } else {
            let path = &runtimes[index];
            vec![(
                zup_artifact::MediaType::RUNTIME,
                std::fs::read(path).map_err(|error| {
                    crate::failure::error(
                        "zup.publish.stage_runtime_unreadable",
                        format!("runtime template `{}`: {error}", path.display()),
                    )
                })?,
            )]
        };
        if let Some(Some(preset)) = loaded.presets.get(index) {
            natives.push((zup_artifact::MediaType::PRESET, preset.clone()));
        }
        variants.push(
            zup_artifact::DistributionVariant::resolve(config, plan, &[], &natives).map_err(
                |error| {
                    crate::failure::error(
                        "zup.publish.stage_variant_invalid",
                        format!("variant `{}`: {error}", config.profile),
                    )
                },
            )?,
        );
    }
    reporter.phase("compose", "→ Composing the release graph");

    let request = zup_artifact::ArtifactRequest::universal_offline(
        format!("{}-web", loaded.manifest.app.id.as_str()),
        &loaded.manifest.app,
        "release",
    );
    let borrowed: Vec<&zup_artifact::DistributionVariant> = variants.iter().collect();
    let graph = crate::artifacts::compose(request, &borrowed).map_err(|error| {
        crate::failure::error("zup.publish.stage_graph", format!("release graph: {error}"))
    })?;

    let downloads = args
        .download
        .iter()
        .map(|path| {
            let size = std::fs::metadata(path)
                .map_err(|error| {
                    crate::failure::error(
                        "zup.publish.download_missing",
                        format!("`{}`: {error}", path.display()),
                    )
                })?
                .len();
            Ok(zup_artifact::ReleaseFile {
                path: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        crate::failure::error(
                            "zup.publish.download_unnamed",
                            format!("`{}` has no file name", path.display()),
                        )
                    })?
                    .to_owned(),
                digest: project::digest_of(path)?,
                size,
                kind: zup_acquire::ReleaseDownloadKind::OfflineInstaller,
                variant: None,
            })
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let channel = zup_artifact::WebExport::new(&args.channel).map_err(|error| {
        crate::failure::error("zup.publish.channel_invalid", format!("channel: {error}"))
    })?;
    let tree = zup_artifact::export_web_tree_with(&graph, &channel, &args.output, &downloads)
        .map_err(|error| {
            crate::failure::error(
                "zup.publish.stage_failed",
                format!("staging the web tree: {error}"),
            )
        })?;

    reporter.log(LogLevel::Info, stage_text(&loaded, &tree, &args));
    reporter.log(LogLevel::Info, tuf_instructions(&tree.root));

    let mut packages = Vec::new();
    let mut thin_installers = Vec::new();
    if thin {
        thin_installers =
            stage_thin_installers(&args, &loaded, &borrowed, &tree.root, &resolver, &reporter)?;
    }
    if let Some(directory) = &args.packages {
        packages = stage_packages(&args, &loaded, &graph, &tree, directory, &reporter)?;
    }
    let mut release_manifest = None;
    if let Some(release_dir) = &args.release_dir {
        release_manifest = Some(crate::automation::project_path(&compose_release(
            release_dir,
            &loaded.manifest.app,
            &reporter,
        )?));
    }
    let staged = PublishStageDetails {
        web_root: crate::automation::project_path(&tree.root),
        channel: args.channel.clone(),
        objects: count(tree.blob_count),
        object_bytes: ByteCount::new(tree.blob_bytes),
        variants: count(tree.variant_count),
        tuf_inputs: count_of(&tree.tuf_targets),
        release_digest: Some(tree.release_digest.to_hex()),
        packages,
        thin_installers,
        release_manifest,
    };
    let staged_summary = format!(
        "{} objects · {} variant(s)",
        staged.objects, staged.variants
    );
    Ok(
        AutomationResult::new(zup_automation::OPERATION_PUBLISH_STAGE)
            .with_application(crate::automation::application(&loaded.manifest.app))
            .with_targets(crate::automation::targets(&loaded.selected_targets))
            .with_artifacts(staged.thin_installers.clone())
            .with_details(Details::PublishStage(staged))
            .with_summary(staged_summary),
    )
}

fn count(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

fn count_of<T>(values: &[T]) -> u32 {
    u32::try_from(values.len()).unwrap_or(u32::MAX)
}

fn stage_text(
    loaded: &LoadedProject,
    tree: &zup_artifact::WebTree,
    args: &PublishStageCommand,
) -> String {
    format!(
        "Staged {} {} ({})\n  Tree        {}\n  Content     {} objects · {}\n  Variants    {}\n  \
         Release     {}\n  TUF targets {} in {}/tuf-input",
        loaded.manifest.app.name,
        loaded.manifest.app.version,
        args.channel,
        tree.root.display(),
        tree.blob_count,
        zup_presentation::format_bytes(tree.blob_bytes),
        tree.variant_count,
        tree.release_digest,
        tree.tuf_targets.len(),
        tree.root.display(),
    )
}

fn tuf_instructions(root: &Path) -> String {
    format!(
        "\nPublish the tree as static files, then sign the release graph:\n  tuftool update \
         --root <trusted-root> --key <signing-key> \\\n    --add-targets {}/tuf-input \\\n    \
         --targets-expires 'in 3 weeks' --snapshot-expires 'in 3 weeks' \\\n    --timestamp-expires 'in 1 \
         week' --outdir <repository>",
        root.display()
    )
}

fn resolve_runtimes(
    loaded: &LoadedProject,
    resolver: &ToolchainResolver,
) -> miette::Result<Vec<PathBuf>> {
    let mut resolved = Vec::with_capacity(loaded.selected_targets.len());
    for config in &loaded.selected_targets {
        let component = toolchain::runtime_for(&config.target, config.frontend);
        match resolver.resolve(&component, None) {
            Ok(found) => resolved.push(found.path),
            Err(error) => {
                return Err(crate::failure::error_with_help(
                    "zup.toolchain.component_missing",
                    format!(
                        "`{}` cannot be staged: {}",
                        config.profile,
                        toolchain::missing_component_message(&component, &error)
                    ),
                    crate::doctor::TOOLCHAIN_HINT,
                ));
            }
        }
    }
    Ok(resolved)
}

fn plan_only_runtime(
    template: &Path,
    plan: &zup_build::TargetBuildPlan,
    config: &zup_core::ResolvedTargetConfig,
) -> miette::Result<Vec<u8>> {
    if config.target.operating_system() == zup_core::TargetOperatingSystem::Linux {
        return Err(crate::failure::error_with_help(
            "zup.publish.linux_thin_unsupported",
            format!(
                "thin releases are not supported for Linux target `{}` (`{}`): the Linux \
                 backend ships self-contained installers with no online launcher",
                config.profile, config.target
            ),
            "Publish without `--thin`.",
        ));
    }
    #[cfg(not(windows))]
    {
        let _ = (template, plan);
        Err(crate::failure::error(
            "zup.publish.unsupported_host",
            "building a thin runtime requires a Windows build host",
        ))
    }
    #[cfg(windows)]
    {
        zup_windows::plan_only_runtime_bytes(template, plan, &[])
            .map(|(bytes, _)| bytes)
            .map_err(|error| {
                crate::failure::error(
                    "zup.publish.thin_runtime_failed",
                    format!("building the thin runtime: {error}"),
                )
            })
    }
}

fn stage_thin_installers(
    args: &PublishStageCommand,
    loaded: &LoadedProject,
    variants: &[&zup_artifact::DistributionVariant],
    tree: &Path,
    resolver: &ToolchainResolver,
    reporter: &Reporter,
) -> miette::Result<Vec<Artifact>> {
    if let Some(config) = loaded
        .selected_targets
        .iter()
        .find(|config| config.target.operating_system() == zup_core::TargetOperatingSystem::Linux)
    {
        return Err(crate::failure::error_with_help(
            "zup.publish.linux_thin_unsupported",
            format!(
                "thin installers are not supported for Linux target `{}` (`{}`): the Linux \
                 backend ships self-contained installers with no online launcher",
                config.profile, config.target
            ),
            "Publish without `--thin`.",
        ));
    }
    #[cfg(windows)]
    {
        stage_thin_windows(args, loaded, variants, tree, resolver, reporter)
    }
    #[cfg(not(windows))]
    {
        let _ = (args, loaded, variants, tree, resolver, reporter);
        Err(crate::failure::error(
            "zup.publish.unsupported_host",
            "staging thin installers requires a Windows build host",
        ))
    }
}

/// branch: a non-Windows host never compiles it, and the dispatcher above
#[cfg(windows)]
fn stage_thin_windows(
    args: &PublishStageCommand,
    loaded: &LoadedProject,
    variants: &[&zup_artifact::DistributionVariant],
    tree: &Path,
    resolver: &ToolchainResolver,
    reporter: &Reporter,
) -> miette::Result<Vec<Artifact>> {
    let updates = loaded
        .build
        .targets
        .first()
        .and_then(|target| target.installer.updates.as_ref())
        .ok_or_else(|| {
            crate::failure::error_with_help(
                "zup.publish.thin_needs_updates",
                "a thin release needs `[updates]` in the manifest: it is where the repository, \
                 the channel, and the trusted root come from",
                "Add an `[updates]` section, or publish without `--thin`.",
            )
        })?;
    let repository = args.repository.clone().unwrap_or_else(|| {
        let absolute = std::fs::canonicalize(tree).unwrap_or_else(|_| tree.to_path_buf());
        let rendered = absolute.display().to_string().replace('\\', "/");
        format!("file:///{}", rendered.trim_start_matches('/'))
    });
    if !updates.channel.is_empty() && updates.channel != args.channel {
        return Err(crate::failure::error_with_help(
            "zup.publish.channel_mismatch",
            format!(
                "`--channel {}` does not match the manifest's `[updates] channel {}`; a thin \
                 installer and the release it installs would read different documents",
                args.channel, updates.channel
            ),
            "Stage the channel the manifest declares, or change `[updates] channel`.",
        ));
    }
    let app_id = loaded.manifest.app.id.clone();
    let output = args.thin_output.clone().unwrap_or_else(|| {
        args.output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    std::fs::create_dir_all(&output).map_err(|error| {
        crate::failure::error(
            "zup.publish.output_unwritable",
            format!("`{}`: {error}", output.display()),
        )
    })?;
    let name: String = loaded
        .manifest
        .app
        .name
        .as_str()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '/')
        .collect();

    let scope = match loaded.manifest.install.scope {
        InstallScope::User => zup_acquire::ThinScope::User,
        InstallScope::Machine => zup_acquire::ThinScope::Machine,
        InstallScope::Either => {
            return Err(crate::failure::error_with_help(
                "zup.publish.thin_scope_ambiguous",
                "a thin installer cannot be built for `install.scope = \"either\"`: a user \
                 double-clicking it has no way to choose",
                "Set the scope to `user` or `machine`, or publish an offline installer.",
            ));
        }
    };

    let mut written: Vec<(&str, PathBuf)> = Vec::new();
    for (label, pin) in [
        (
            "version",
            zup_acquire::ReleasePin::Version {
                version: loaded.manifest.app.version.to_string(),
            },
        ),
        (
            "channel",
            zup_acquire::ReleasePin::Channel {
                channel: args.channel.clone(),
            },
        ),
    ] {
        let trust = zup_acquire::OnlineTrust::new(
            app_id.clone(),
            &args.channel,
            &repository,
            &updates.trusted_root,
            pin,
        )
        .with_scope(scope);
        let request = zup_artifact::ArtifactRequest::thin_online(
            format!("{app_id}-{label}"),
            &loaded.manifest.app,
            trust,
            format!("{name}-Setup-{label}.exe"),
        );
        let graph = crate::artifacts::compose(request, variants).map_err(|error| {
            crate::failure::error(
                "zup.publish.thin_graph_failed",
                format!("thin artifact graph: {error}"),
            )
        })?;
        let component = toolchain::dispatcher_for(graph.index().artifact.subsystem, args.thin);
        let dispatcher = resolver
            .resolve(&component, args.dispatcher.as_deref())
            .map_or_else(
                |error| {
                    Err(crate::failure::error_with_help(
                        "zup.toolchain.component_missing",
                        format!(
                            "a thin installer cannot be composed: {}",
                            toolchain::missing_component_message(&component, &error)
                        ),
                        crate::doctor::TOOLCHAIN_HINT,
                    ))
                },
                |resolved| Ok(resolved.path),
            )?;
        let file = output.join(&graph.index().artifact.output);
        #[cfg(windows)]
        zup_windows::compose_universal_executable(&dispatcher, &file, &graph).map_err(|error| {
            crate::failure::error(
                "zup.publish.thin_unwritable",
                format!("`{}`: {error}", file.display()),
            )
        })?;
        #[cfg(not(windows))]
        {
            let _ = (&dispatcher, &file, &graph);
        }
        crate::build::stamp_application_icon(&file, &loaded.build)?;
        written.push((label, file));
    }

    reporter.log(
        LogLevel::Info,
        format!(
            "\nThin installers\n{}\n\nThe two differ only in which document they authenticate:\n  \
             version  releases/{}/versions/{}.json - the release it was built for\n  channel  \
             releases/{}.json - whatever the channel currently says",
            written
                .iter()
                .map(|(label, file)| {
                    let size = std::fs::metadata(file).map(|item| item.len()).unwrap_or(0);
                    format!(
                        "  {label:<8} {} · {}",
                        file.display(),
                        zup_presentation::format_bytes(size)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            args.channel,
            loaded.manifest.app.version,
            args.channel,
        ),
    );
    Ok(written
        .into_iter()
        .map(|(label, file)| {
            let size = std::fs::metadata(&file).map(|item| item.len()).unwrap_or(0);
            Artifact {
                path: crate::automation::release_path(label),
                digest: project::digest_of(&file)
                    .map(|d| zup_automation::Digest::sha256(d.to_hex()))
                    .unwrap_or_else(|_| zup_automation::Digest::sha256("0".repeat(64))),
                size: ByteCount::new(size),
                kind: zup_automation::Identifier::fixed("thin"),
                mode: zup_automation::Identifier::fixed("thin"),
                id: Some(format!("{}-{}", loaded.manifest.app.id, label)),
                target: None,
                variants: None,
                signing: None,
            }
        })
        .collect())
}

fn stage_packages(
    args: &PublishStageCommand,
    loaded: &LoadedProject,
    graph: &zup_artifact::ArtifactGraph,
    tree: &zup_artifact::WebTree,
    output: &Path,
    reporter: &Reporter,
) -> miette::Result<Vec<StagedPackage>> {
    let channel = args.channel.as_str();
    let catalog_bytes = std::fs::read(
        tree.root.join(
            zup_acquire::WebLayout::catalog(channel)
                .map_err(|error| {
                    crate::failure::error("zup.publish.catalog_unreadable", error.to_string())
                })?
                .to_string(),
        ),
    )
    .map_err(|error| {
        crate::failure::error_with_help(
            "zup.publish.catalog_missing",
            "the content catalog is missing from the staged tree",
            "Re-run `zup publish stage`.",
        )
        .wrap_err(error)
    })?;
    let catalog = zup_acquire::ContentCatalog::parse(&catalog_bytes).map_err(|error| {
        crate::failure::error(
            "zup.publish.catalog_invalid",
            format!("the content catalog is not readable: {error}"),
        )
    })?;
    let catalog_document = zup_distribute_github::Document {
        digest: zup_core::hash_bytes(&catalog_bytes),
        size: catalog_bytes.len() as u64,
    };
    let shard_bytes = args
        .shard_bytes
        .unwrap_or(zup_publish_github::PACKAGE_SHARD_BYTES);
    let name = loaded
        .manifest
        .app
        .name
        .as_str()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '/')
        .collect::<String>();
    let mut packages = Vec::new();
    let mut rendered = vec!["\nTransport packages".to_owned()];
    for variant in graph.manifests() {
        let manifest_bytes = &variant.bytes;
        let document = zup_distribute_github::Document {
            digest: zup_core::hash_bytes(manifest_bytes),
            size: manifest_bytes.len() as u64,
        };
        let Some(content) = graph.content_of(&variant.id) else {
            continue;
        };
        let blobs: Vec<(zup_core::Sha256Digest, u64, u64)> = content
            .digests
            .iter()
            .filter_map(|digest| {
                catalog
                    .entry(digest)
                    .map(|entry| (*digest, entry.compressed_size, entry.size))
            })
            .collect();
        let request = crate::packages::Request {
            variant: &variant.id,
            target: graph
                .index()
                .variants
                .iter()
                .find(|described| described.id == variant.id)
                .map(|described| described.target.as_str())
                .unwrap_or_default(),
            application: loaded.manifest.app.id.as_str(),
            manifest: document,
            catalog: catalog_document,
            blobs: &blobs,
        };
        let written = crate::packages::write(&request, &tree.root, output, &name, shard_bytes)?;
        rendered.push(format!(
            "  {:<24} {} · {} objects · {}",
            variant.id,
            zup_publish::format_bytes(written.size),
            written.blob_count,
            written
                .names
                .iter()
                .map(|piece| piece.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        packages.push(StagedPackage {
            variant: variant.id.clone(),
            names: written.names,
            size: ByteCount::new(written.size),
            blob_count: count(written.blob_count),
        });
    }
    reporter.log(LogLevel::Info, rendered.join("\n"));
    Ok(packages)
}

fn compose_release(
    release_dir: &Path,
    app: &zup_core::App,
    reporter: &Reporter,
) -> miette::Result<PathBuf> {
    let mut composed = zup_artifact::ReleaseManifest::new(app);
    let mut variants = 0usize;
    let mut entries = std::fs::read_dir(release_dir.join("variants"))
        .map_err(|error| {
            crate::failure::error_with_help(
                "zup.publish.variants_directory_missing",
                format!("`{}/variants`: {error}", release_dir.display()),
                "The compose job collects each matrix job's output in `<release-dir>/variants`.",
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            crate::failure::error(
                "zup.publish.variants_unreadable",
                format!("`{}/variants`: {error}", release_dir.display()),
            )
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let directory = entry.path();
        if !directory.is_dir() {
            continue;
        }
        let description = directory.join(zup_artifact::RELEASE_MANIFEST_NAME);
        let Ok(bytes) = std::fs::read(&description) else {
            continue;
        };
        let part = zup_artifact::ReleaseManifest::parse(&bytes).map_err(|error| {
            crate::failure::error(
                "zup.publish.variant_description_invalid",
                format!("`{}`: {error}", description.display()),
            )
        })?;
        for variant in &part.variants {
            composed.add_variant(variant).map_err(|error| {
                crate::failure::error(
                    "zup.publish.variant_rejected",
                    format!("`{}`: {error}", description.display()),
                )
            })?;
            variants += 1;
        }
        for artifact in &part.artifacts {
            let path = join_release_path(release_dir, &artifact.path)?;
            let size = std::fs::metadata(&path)
                .map(|meta| meta.len())
                .map_err(|error| {
                    crate::failure::error(
                        "zup.publish.artifact_missing",
                        format!(
                            "`{}` claims `{}` but the compose job could not see it: {error}",
                            description.display(),
                            path.display()
                        ),
                    )
                })?;
            if size != artifact.built.size {
                return Err(crate::failure::error(
                    "zup.publish.size_mismatch",
                    format!(
                        "`{}` is {size} bytes and the release description says {}",
                        path.display(),
                        artifact.built.size
                    ),
                ));
            }
            composed.add_composed(artifact, &path).map_err(|error| {
                crate::failure::error(
                    "zup.publish.artifact_rejected",
                    format!("`{}`: {error}", path.display()),
                )
            })?;
        }
    }
    if variants == 0 {
        return Err(crate::failure::error_with_help(
            "zup.publish.no_variants",
            format!(
                "`{}/variants` holds no release descriptions",
                release_dir.display()
            ),
            "The build matrix writes one release description per target and the compose job \
             reads them all.",
        ));
    }
    let path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    let bytes = composed.encode().map_err(|error| {
        crate::failure::error(
            "zup.publish.release_unencodable",
            format!("composing the release description: {error}"),
        )
    })?;
    zup_platform::publish(&path, &bytes).map_err(|error| {
        crate::failure::error(
            "zup.publish.release_unwritable",
            format!("`{}`: {error}", path.display()),
        )
    })?;
    reporter.log(
        LogLevel::Info,
        format!(
            "\nComposed {} {} ({} files · {variants} variants)\n  Release     {}",
            app.name,
            app.version,
            composed.artifacts.len(),
            path.display()
        ),
    );
    Ok(path)
}

fn join_release_path(root: &Path, relative: &str) -> miette::Result<PathBuf> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/') || relative.contains("..") || relative.contains(':') {
        return Err(crate::failure::error(
            "zup.publish.path_escapes_release",
            format!("`{relative}` is not a path inside the release root"),
        ));
    }
    Ok(root.join(relative))
}

pub fn run_github(args: PublishGithubCommand) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let manifest = project::load_manifest(&args.manifest)?;
    let mut config = zup_publish_github::PublishConfig::resolve(&manifest)?;
    if let Some(tag) = &args.tag {
        config.tag = Some(tag.clone());
        config.tag_prefix = None;
    }
    if args.draft {
        config.draft = true;
    }
    if args.prerelease {
        config.prerelease = true;
    }
    if args.replace_conflicts {
        config.replace_conflicts = true;
    }
    let working = args
        .manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (repository, how) =
        crate::publish::resolve_repository(&config, args.repo.as_deref(), &working)?;
    let token = match zup_publish_github::discover(&zup_publish_github::ProcessEnvironment) {
        Ok(token) => token,
        Err(error) => {
            if !args.dry_run {
                return Err(crate::failure::error_with_help(
                    "zup.publish.no_credential",
                    error.to_string(),
                    "Pass a token in GH_TOKEN or GITHUB_TOKEN, or authenticate `gh auth login`.",
                ));
            }
            reporter.log(
                LogLevel::Warning,
                format!(
                    "  - no credential: {error}\n  - the plan below is complete; publication was \
                     not attempted"
                ),
            );
            return Ok(no_credential_result(
                &manifest,
                &config,
                &args,
                &repository,
                error.to_string(),
            ));
        }
    };
    let staged = crate::publish::Staged {
        release_dir: args.release_dir.clone(),
        web: args.web.clone(),
        packages: args.packages.clone(),
    };
    let options = crate::publish::Options {
        dry_run: args.dry_run,
        draft: None,
        prerelease: None,
        receipt: args.receipt.clone(),
        notes_text: args.notes_text.clone(),
    };
    let report = crate::publish::run(&manifest, &config, &staged, &repository, &token, &options)?;
    reporter.log(LogLevel::Info, report.human());
    reporter.log(LogLevel::Info, format!("repository  {repository} ({how})"));

    let receipt = args
        .receipt
        .as_ref()
        .map(|path| crate::automation::project_path(path));
    let publication = crate::automation::publication(&report, receipt.as_deref());
    let failures = report
        .failures()
        .into_iter()
        .map(|step| zup_automation::PublishFailure {
            phase: phase_of(&report, step),
            step: step.label.clone(),
            detail: step
                .detail
                .clone()
                .unwrap_or_else(|| step.status.as_str().to_owned()),
        })
        .collect::<Vec<_>>();
    let mut result = AutomationResult::new(zup_automation::OPERATION_PUBLISH_GITHUB)
        .with_application(crate::automation::application(&manifest.app))
        .with_publication(publication.clone())
        .with_details(Details::Publish(zup_automation::PublishDetails {
            dry_run: args.dry_run,
            receipt,
            failures: failures.clone(),
        }))
        .with_summary(format!(
            "{} {} with {} asset(s)",
            if args.dry_run { "Planned" } else { "Published" },
            publication.tag,
            publication.assets.len()
        ));
    if !report.is_complete() {
        let mut failed = result.failed();
        for failure in &failures {
            failed = failed.with_diagnostic(
                zup_automation::Diagnostic::error("zup.publish.incomplete", failure.detail.clone())
                    .with_help(format!("{} · {}", failure.phase, failure.step)),
            );
        }
        if failures.is_empty() {
            failed = failed.with_diagnostic(zup_automation::Diagnostic::error(
                "zup.publish.incomplete",
                format!("publishing {} did not finish", report.tag),
            ));
        }
        result = failed;
    }
    Ok(result)
}

fn phase_of(report: &zup_publish::PublishReport, step: &zup_publish::StepReport) -> String {
    report
        .phases
        .iter()
        .find(|phase| phase.steps.iter().any(|entry| entry.label == step.label))
        .map(|phase| phase.name.clone())
        .unwrap_or_else(|| "Publishing".to_owned())
}

fn no_credential_result(
    manifest: &zup_manifest::Manifest,
    config: &zup_publish_github::PublishConfig,
    args: &PublishGithubCommand,
    repository: &zup_publish_github::GithubRepository,
    reason: String,
) -> AutomationResult {
    let tag = crate::publish::preview_tag(manifest, config);
    let summary = format!("Planned a publication to {tag}; nothing was written");
    AutomationResult::new(zup_automation::OPERATION_PUBLISH_GITHUB)
        .with_application(crate::automation::application(&manifest.app))
        .with_diagnostic(zup_automation::Diagnostic {
            severity: zup_automation::Severity::Warning,
            code: zup_automation::Identifier::fixed("zup.publish.no_credential"),
            message: reason,
            source: None,
            help: Some(
                "Pass a token in GH_TOKEN or GITHUB_TOKEN, or authenticate `gh auth login`."
                    .to_owned(),
            ),
        })
        .with_publication(Publication {
            provider: "github".to_owned(),
            subject: format!("{}/{}", repository.owner, repository.name),
            tag,
            id: None,
            state: zup_automation::Identifier::fixed("planned"),
            url: None,
            immutable: None,
            assets: Vec::new(),
            receipt: args
                .receipt
                .as_ref()
                .map(|path| crate::automation::project_path(path)),
        })
        .with_details(Details::Publish(zup_automation::PublishDetails {
            dry_run: true,
            receipt: args
                .receipt
                .as_ref()
                .map(|path| crate::automation::project_path(path)),
            failures: Vec::new(),
        }))
        .with_summary(summary)
}

use std::collections::BTreeMap;

use zup_publish::{
    Application, HostLimits, ProductClass, ProductRole, ReleasePlan, ReleaseProduct, SourceClaim,
    TagIntent, TagPolicy,
};
use zup_publish_github::{
    GithubError, GithubRepository, LIMITS, MatrixTarget, NotesPolicy, ProcessEnvironment,
    PublishConfig, PublishRequest, RepositorySpec, WorkflowPolicy, find_git_config, publish,
};

fn read_release(release_dir: &Path) -> miette::Result<zup_artifact::ReleaseManifest> {
    let path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    let bytes = std::fs::read(&path).map_err(|error| {
        miette::miette!(
            "`{}` could not be read: {error}; run `zup build` before publishing",
            path.display()
        )
    })?;
    zup_artifact::ReleaseManifest::parse(&bytes).map_err(|error| {
        miette::miette!("`{}` is not a release description: {error}", path.display())
    })
}

#[derive(Debug, Clone, Default)]
pub struct Staged {
    pub release_dir: PathBuf,
    pub web: Option<PathBuf>,
    pub packages: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub draft: Option<bool>,
    pub prerelease: Option<bool>,
    pub receipt: Option<PathBuf>,
    pub notes_text: Option<String>,
}

pub fn build_plan(
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
    release: &zup_artifact::ReleaseManifest,
    staged: &Staged,
    tag: &TagIntent,
    limits: &HostLimits,
) -> miette::Result<ReleasePlan> {
    if !release.is_finalized() {
        let unfinalized = release.unfinalized();
        return Err(miette::miette!(
            "this release has not been signed and finalized: {} still carry pre-signature \
             digests. Sign the files in `{}` and run `zup sign verify` before publishing.",
            if unfinalized.len() == 1 {
                format!("`{}` does", unfinalized[0])
            } else {
                format!("{} do", unfinalized.join("`, `"))
            },
            zup_signing::SIGNING_PLAN_NAME
        ));
    }
    let release_dir = staged.release_dir.as_path();
    let web = staged.web.as_deref();
    let packages = staged.packages.as_deref();
    let mut plan = ReleasePlan::new(
        Application::new(
            release.application.id.clone(),
            release.application.name.as_str(),
            release.application.version.to_string(),
        ),
        tag.clone(),
    )
    .with_source(SourceClaim::none());

    for artifact in &release.artifacts {
        let path = join_release(release_dir, &artifact.path)?;
        let (size, digest) = measure(&path)?;
        let expected = release.published_digest(artifact);
        if digest != expected || size != release.published_size(artifact) {
            let (which, expected_digest, expected_size) = match &artifact.finalized {
                Some(finalized) => (
                    "the finalized release description",
                    *finalized.digest(),
                    finalized.size(),
                ),
                None => (
                    "the release description",
                    artifact.built.digest,
                    artifact.built.size,
                ),
            };
            return Err(miette::miette!(
                "`{}` is sha256:{} at {size} bytes, and {which} says sha256:{expected_digest} at \
                 {expected_size} bytes",
                path.display(),
                digest.to_hex()
            ));
        }
        let product = ReleaseProduct::new(
            asset_name(&artifact.path)?,
            ProductRole::Install,
            ProductClass::UserFacing,
            digest,
            size,
        )
        .with_media_type(media_type_for(&artifact.path))
        .serving(std::iter::once(artifact.id.clone()));
        plan.push(product);
    }

    let manifest_path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    plan.push(
        ReleaseProduct::new(
            zup_artifact::RELEASE_MANIFEST_NAME,
            ProductRole::Manifest,
            ProductClass::UserFacing,
            digest_of(&manifest_path)?,
            size_of(&manifest_path)?,
        )
        .with_media_type("application/json")
        .serving(["release".to_owned()]),
    );

    if let Some(web) = web
        && web.is_dir()
    {
        for (document, path) in documents(web)? {
            plan.push(
                ReleaseProduct::new(
                    document,
                    ProductRole::Manifest,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/json"),
            );
        }
    }

    if let Some(packages) = packages
        && packages.is_dir()
    {
        for (document, path) in package_documents(packages)? {
            plan.push(
                ReleaseProduct::new(
                    document,
                    ProductRole::Manifest,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/json"),
            );
        }
        for (name, path) in package_files(packages)? {
            plan.push(
                ReleaseProduct::new(
                    name,
                    ProductRole::Auxiliary,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/vnd.zup.package.v1"),
            );
        }
    }

    if let Some(origin) = content_origin(manifest, config) {
        plan = plan.with_origins(vec![origin]);
    }
    plan.preflight(limits)
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(plan)
}

fn content_origin(
    _manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
) -> Option<zup_publish::ContentOrigin> {
    let repository = config.repository.as_ref()?;
    let host = config.install();
    let path = format!("{}/{}", repository.owner, repository.name);
    let base = host.latest_download_url(&path, "");
    Some(zup_publish_github::content_origin(
        config.distribution,
        base,
    ))
}

fn documents(web: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    collect_documents(web, web, &mut out)?;
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

fn collect_documents(
    root: &Path,
    directory: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> miette::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(miette::miette!("`{}`: {error}", directory.display()));
        }
    };
    for entry in entries {
        let path = entry
            .map_err(|error| miette::miette!("`{}`: {error}", directory.display()))?
            .path();
        if path.is_dir() {
            collect_documents(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| miette::miette!("`{}` is not under the staged tree", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("blobs/") {
            continue;
        }
        let Some(parsed) = relative.parse::<zup_acquire::RelativeContentPath>().ok() else {
            continue;
        };
        let name =
            zup_publish::asset_name(&parsed).map_err(|reason| miette::miette!("{reason}"))?;
        out.push((name, path));
    }
    Ok(())
}

fn package_documents(packages: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for (name, path) in package_files(packages)? {
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let variant = stem
            .rsplit_once('-')
            .map(|(_, variant)| variant)
            .unwrap_or(stem);
        out.push((
            zup_publish::document_name(&zup_publish::DocumentKind::Package { variant }),
            path,
        ));
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

fn package_files(packages: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(packages)
        .map_err(|error| miette::miette!("`{}`: {error}", packages.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| miette::miette!("`{}`: {error}", packages.display()))?
            .path();
        if !path.is_file() {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| miette::miette!("`{}` has no file name", path.display()))?;
        if !file.ends_with(".zup") {
            continue;
        }
        zup_publish::check_asset_name(file).map_err(|reason| {
            miette::miette!("`{file}` cannot be a release asset name: {reason}")
        })?;
        out.push((file.to_owned(), path));
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

pub fn resolve_tag(
    version: &str,
    config: &PublishConfig,
    override_tag: Option<&str>,
) -> miette::Result<TagIntent> {
    if let Some(tag) = override_tag {
        if tag.trim().is_empty() {
            return Err(miette::miette!("`--tag` was given an empty tag"));
        }
        return Ok(if config.create_tag {
            TagIntent::creatable(
                tag.trim(),
                TagPolicy::Exact {
                    tag: tag.trim().into(),
                },
            )
        } else {
            TagIntent::required(tag.trim())
        });
    }
    if let Some(tag) = &config.tag {
        return Ok(TagIntent::required(tag));
    }
    let policy = config.tag_policy();
    let tag = policy
        .derive(version)
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(if config.create_tag {
        TagIntent::creatable(tag, policy)
    } else {
        TagIntent::required(tag)
    })
}

pub fn resolve_repository(
    config: &PublishConfig,
    explicit: Option<&str>,
    working_directory: &Path,
) -> miette::Result<(GithubRepository, String)> {
    let spec = match explicit {
        Some(value) => Some(RepositorySpec::parse(value).map_err(failed)?),
        None => config.repository.clone(),
    };
    let environment = ProcessEnvironment;
    let root = find_git_config(working_directory)
        .and_then(|git| git.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| working_directory.to_path_buf());
    let resolved =
        zup_publish_github::resolve(spec.as_ref(), &environment, &root).map_err(failed)?;
    Ok((
        resolved.repository,
        format!(
            "{} ({})",
            resolved.discovery.as_str(),
            resolved
                .remote
                .as_deref()
                .map(|remote| format!("remote `{remote}`"))
                .unwrap_or_default()
        ),
    ))
}

fn failed(error: GithubError) -> miette::Report {
    miette::miette!("{error}")
}

pub fn run(
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
    staged: &Staged,
    repository: &GithubRepository,
    token: &zup_publish_github::Token,
    options: &Options,
) -> miette::Result<zup_publish::PublishReport> {
    let release_dir = staged.release_dir.as_path();
    let web = staged.web.as_deref();
    let packages = staged.packages.as_deref();
    let release = read_release(release_dir)?;
    let tag = resolve_tag(&release.application.version.to_string(), config, None)?;
    let plan = build_plan(manifest, config, &release, staged, &tag, &LIMITS)?;
    let sources = locate_sources(&plan, release_dir, web, packages)?;

    let mut request = PublishRequest::new(plan);
    request.sources = sources;
    request.notes = config.notes.clone();
    request.notes_file = match &config.notes {
        NotesPolicy::File(path) => Some(PathBuf::from(path)),
        _ => None,
    };
    request.notes_text = options
        .notes_text
        .clone()
        .or_else(|| config.notes_text.clone());
    request.dry_run = options.dry_run;
    request.draft = options.draft.unwrap_or(config.draft);
    request.prerelease = options.prerelease.unwrap_or(config.prerelease);
    request.replace_conflicts = config.replace_conflicts;
    request.receipt = options.receipt.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("a runtime for the publish request: {error}"))?;
    runtime
        .block_on(publish(repository, token, &request))
        .map_err(failed)
}

fn locate_sources(
    plan: &ReleasePlan,
    release_dir: &Path,
    web: Option<&Path>,
    packages: Option<&Path>,
) -> miette::Result<BTreeMap<String, PathBuf>> {
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for role in ProductRole::ALL {
        for product in plan.role(role) {
            if by_name.contains_key(&product.name) {
                continue;
            }
            let path = match product.name.as_str() {
                zup_artifact::RELEASE_MANIFEST_NAME => {
                    release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME)
                }
                name if name.ends_with(".zup") || name.contains(".zup.") => {
                    let packages = packages.ok_or_else(|| {
                        miette::miette!(
                            "`{name}` is a transport package but no package directory was given"
                        )
                    })?;
                    packages.join(name)
                }
                name => {
                    let mut candidates = vec![release_dir.join(name)];
                    if let Some(web) = web {
                        candidates.push(web.join(name));
                    }
                    match candidates.into_iter().find(|path| path.is_file()) {
                        Some(found) => found,
                        None => search_tree(web, name, &product.name)?,
                    }
                }
            };
            if !path.is_file() {
                return Err(miette::miette!(
                    "the release plan names `{}` but `{}` does not exist",
                    product.name,
                    path.display()
                ));
            }
            by_name.insert(product.name.clone(), path);
        }
    }
    Ok(by_name)
}

fn search_tree(web: Option<&Path>, asset: &str, original: &str) -> miette::Result<PathBuf> {
    let Some(web) = web else {
        return Err(miette::miette!(
            "the release plan names `{original}`, which is not in the release directory"
        ));
    };
    let mut found = None;
    let mut stack = vec![web.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if found.is_some() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(web) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.starts_with("blobs/") {
                continue;
            }
            let Ok(parsed) = relative.parse::<zup_acquire::RelativeContentPath>() else {
                continue;
            };
            if zup_publish::asset_name(&parsed).ok().as_deref() == Some(asset) {
                found = Some(path);
            }
        }
    }
    found.ok_or_else(|| {
        miette::miette!("the release plan names `{original}`, which is not in the staged tree")
    })
}

pub fn matrix(manifest: &zup_manifest::Manifest) -> Vec<MatrixTarget> {
    manifest
        .build
        .targets
        .iter()
        .map(|(profile, target)| MatrixTarget::new(profile.as_str(), target.target.as_str()))
        .collect()
}

pub fn workflow(
    root: &Path,
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
) -> miette::Result<zup_publish_github::Freshness> {
    let policy: &WorkflowPolicy = &config.workflow;
    let targets = matrix(manifest);
    Ok(zup_publish_github::check(root, policy, &targets))
}

pub fn render_workflow(manifest: &zup_manifest::Manifest, config: &PublishConfig) -> String {
    zup_publish_github::generate(&config.workflow, &matrix(manifest))
}

pub fn preview_tag(manifest: &zup_manifest::Manifest, config: &PublishConfig) -> String {
    resolve_tag(&manifest.app.version.to_string(), config, None)
        .map(|tag| tag.tag)
        .unwrap_or_else(|_| manifest.app.version.to_string())
}

fn asset_name(path: &str) -> miette::Result<String> {
    let trimmed = path.trim_start_matches("./");
    if let Ok(parsed) = trimmed.parse::<zup_acquire::RelativeContentPath>()
        && let Ok(name) = zup_publish::asset_name(&parsed)
    {
        return Ok(name);
    }
    let name = trimmed.replace('\\', "/").replace('/', "-");
    zup_publish::check_asset_name(&name)
        .map_err(|reason| miette::miette!("`{path}` cannot be a release asset name: {reason}"))?;
    Ok(name)
}

fn media_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("exe") => "application/vnd.microsoft.portable-executable",
        Some("msi") => "application/x-msi",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn join_release(root: &Path, relative: &str) -> miette::Result<PathBuf> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/') || relative.contains("..") || relative.contains(':') {
        return Err(miette::miette!(
            "`{relative}` is not a path inside the release root"
        ));
    }
    Ok(root.join(relative))
}

fn digest_of(path: &Path) -> miette::Result<zup_core::Sha256Digest> {
    let file = std::fs::File::open(path)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    zup_core::hash_reader(std::io::BufReader::new(file))
        .map(|(_, digest)| digest)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))
}

/// from another describes a file that never existed, and the check above is only
fn measure(path: &Path) -> miette::Result<(u64, zup_core::Sha256Digest)> {
    let file = std::fs::File::open(path)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    zup_core::hash_reader(std::io::BufReader::new(file))
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))
}

fn size_of(path: &Path) -> miette::Result<u64> {
    std::fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))
}
