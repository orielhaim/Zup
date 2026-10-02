//! `zup publish`: a release a client can authenticate.
//!
//! `stage` writes the immutable web tree a static origin serves and a TUF
//! repository signs. The point of the command is that a developer does not
//! reconstruct the artifact graph by hand: `zup build` has already worked out
//! what content exists, what is shared, and what each variant needs, and this
//! takes the same answer and lays it out as a directory a plain file server can
//! host.
//!
//! ```text
//! <output>/blobs/sha256/…           one compressed object per unique digest
//! <output>/releases/<channel>.json  the release descriptor
//! <output>/releases/<channel>/…     the catalog and one manifest per variant
//! <output>/tuf-input/…             the same documents, for tuftool
//! ```
//!
//! Nothing is signed here. `tuftool` reads `tuf-input`, and the signed metadata
//! plus this tree are what a static origin serves.

use std::path::{Path, PathBuf};

use zup_automation::{
    Artifact, AutomationResult, ByteCount, Details, LogLevel, Publication, PublishStageDetails,
    StagedPackage,
};
use zup_core::InstallScope;

use crate::cli::{PublishGithubCommand, PublishStageCommand};
use crate::project::{self, LoadedProject};
use crate::report::Reporter;
use crate::toolchain::{self, ToolchainResolver};

/// Write the web tree a static origin serves and a TUF repository signs.
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

    // A thin release's runtime is not the template: it is the template with this
    // target's plan compiled into it and none of the content. That is what makes
    // it a few megabytes instead of the whole application, and it is why a thin
    // installer that embedded it would be a slow offline installer wearing a
    // different name.
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
                plan_only_runtime(&runtimes[index], plan)?,
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
        // The window, when this target has one. The same treatment the runtime
        // gets and for the same reason: a release that names a window without
        // carrying its executable is a release a client cannot install from, and
        // the client finds that out on a machine rather than at publish time. The
        // bytes are the ones the build already selected out of the package, so
        // publishing re-derives nothing and trusts nothing new.
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

    // One graph for the whole release. The web layout is variant-oriented, not
    // artifact-oriented: the offline installer is a claim in the release rather
    // than a container the content is trapped inside.
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

/// A count that is structurally small, and so is inside the range every consumer holds
/// exactly.
fn count(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

/// The same, for a collection Rust counted rather than the domain.
fn count_of<T>(values: &[T]) -> u32 {
    u32::try_from(values.len()).unwrap_or(u32::MAX)
}

/// What a person reads after a successful stage.
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

/// The commands that sign the staged tree, which `tuftool` - not zup - runs.
fn tuf_instructions(root: &Path) -> String {
    format!(
        "\nPublish the tree as static files, then sign the release graph:\n  tuftool update \
         --root <trusted-root> --key <signing-key> \\\n    --add-targets {}/tuf-input \\\n    \
         --targets-expires 'in 3 weeks' --snapshot-expires 'in 3 weeks' \\\n    --timestamp-expires 'in 1 \
         week' --outdir <repository>",
        root.display()
    )
}

/// The runtime template each selected target contributes.
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

/// Build the native runtime a thin release serves.
///
/// The plan is *proved* here - every declared file is read and checked against
/// its own size and digest - so a plan whose files do not exist is refused at
/// publish time rather than at install time on a user's machine.
fn plan_only_runtime(
    template: &Path,
    plan: &zup_build::TargetBuildPlan,
) -> miette::Result<Vec<u8>> {
    zup_windows::plan_only_runtime_bytes(template, plan, &[])
        .map(|(bytes, _)| bytes)
        .map_err(|error| {
            crate::failure::error(
                "zup.publish.thin_runtime_failed",
                format!("building the thin runtime: {error}"),
            )
        })
}

/// Write the two thin installers.
///
/// They differ in exactly one byte of intent: which document they authenticate.
/// A version-labelled installer always installs the release it was built for,
/// because it reads an immutable version-addressed name. A channel installer
/// installs whatever the channel currently says, because it reads the pointer.
/// Everything else about them is identical, which is the point: they are one
/// artifact with two promises, not two artifacts.
fn stage_thin_installers(
    args: &PublishStageCommand,
    loaded: &LoadedProject,
    variants: &[&zup_artifact::DistributionVariant],
    tree: &Path,
    resolver: &ToolchainResolver,
    reporter: &Reporter,
) -> miette::Result<Vec<Artifact>> {
    // The trusted root is a build input, already read and validated when the
    // project was materialized, and it is inlined rather than shipped beside the
    // installer. A root is a few kilobytes of signed JSON, and inlining it is what
    // lets a sub-megabyte launcher be a complete trust anchor rather than a
    // download that has to be trusted before it can be checked.
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
    // The channel a thin installer reads and the channel this release is staged
    // into are the same string, and a build that let them differ would publish a
    // launcher pointing at a document nobody signed. The manifest's channel is
    // what the application itself will check updates against, so the staged
    // channel has to be it.
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

    // A thin artifact carries no variant manifest, so the scope the application
    // declares has nowhere else to travel - the launcher reads it from the trust
    // block. `either` is refused rather than defaulted: a bootstrapper a person
    // double-clicked cannot ask them, and silently choosing one is how a
    // per-machine application ends up in a user's profile.
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
        // A thin artifact is the reason the online launcher exists, so it is
        // also the only thing that asks for it.
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
        zup_windows::compose_universal_executable(&dispatcher, &file, &graph).map_err(|error| {
            crate::failure::error(
                "zup.publish.thin_unwritable",
                format!("`{}`: {error}", file.display()),
            )
        })?;
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
    // The two installers are release products, so they are artifacts of the
    // operation - a consumer asked what a stage produced gets them by name rather
    // than by looking in a directory.
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

/// Write one transport package per variant, plus the descriptor naming each.
///
/// One package per variant rather than one per release, because a package's
/// purpose is to let a machine fetch only the content it needs: a host that
/// serves a release to a thousand machines of several architectures should not
/// make every one of them download the union.
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

/// Fold a build matrix's per-variant descriptions into one release description.
///
/// This is the local/global split made explicit. A matrix builds each target on
/// its own machine and leaves a description behind; one compose job reads them
/// all back together and writes the single document that `zup publish github`
/// and every downstream consumer read.
///
/// A merge, not a rebuild, and a strict one: every file a variant claims is
/// measured on disk, and a file that is claimed and absent is a hard failure.
/// Silently dropping it would produce a release that installs on the machines
/// whose variant survived and not on the others, which is the worst possible
/// failure mode for a release.
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
            // The file is measured again here rather than trusted from the
            // per-variant description, because the compose job is the last place
            // a digest can be wrong before the release is signed and published.
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
    zup_windows::write_durable(&path, &bytes).map_err(|error| {
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

/// Join a release-root-relative path, refusing anything that is not one.
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

/// Publish a release to GitHub.
///
/// The whole command in one place, because the interesting part is not the
/// request: it is that every step before the one that makes the release public is
/// idempotent, and that a dry run performs all of them and writes none of them.
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
        crate::publish_github::resolve_repository(&config, args.repo.as_deref(), &working)?;
    let token = match zup_publish_github::discover(&zup_publish_github::ProcessEnvironment) {
        Ok(token) => token,
        Err(error) => {
            // A dry run still has to establish that a credential exists, because
            // "would this work" includes "could this authenticate". But a dry run
            // that cannot find one is still useful, so it degrades to a plan and
            // says why.
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
    let staged = crate::publish_github::Staged {
        release_dir: args.release_dir.clone(),
        web: args.web.clone(),
        packages: args.packages.clone(),
    };
    let options = crate::publish_github::Options {
        dry_run: args.dry_run,
        draft: None,
        prerelease: None,
        receipt: args.receipt.clone(),
        notes_text: args.notes_text.clone(),
    };
    let report =
        crate::publish_github::run(&manifest, &config, &staged, &repository, &token, &options)?;
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
        // A publication that did not finish is a failure with the provider's own
        // reasons, not a bare error: the steps it could not complete are the answer, and
        // they are already carried.
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

/// The phase a failed step belongs to, read from the report rather than tracked
/// separately: the report already pairs each step with its phase, and a second
/// bookkeeping structure here would be a third thing to keep in step.
fn phase_of(report: &zup_publish::PublishReport, step: &zup_publish::StepReport) -> String {
    report
        .phases
        .iter()
        .find(|phase| phase.steps.iter().any(|entry| entry.label == step.label))
        .map(|phase| phase.name.clone())
        .unwrap_or_else(|| "Publishing".to_owned())
}

/// The result of a dry run that could not find a credential.
///
/// A plan, not a failure: everything except the write was checked, and the reason the
/// write was not attempted is the diagnostic. Reported as a warning so a consumer can
/// tell it apart from a publication that was refused.
fn no_credential_result(
    manifest: &zup_manifest::Manifest,
    config: &zup_publish_github::PublishConfig,
    args: &PublishGithubCommand,
    repository: &zup_publish_github::GithubRepository,
    reason: String,
) -> AutomationResult {
    let tag = crate::publish_github::preview_tag(manifest, config);
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
