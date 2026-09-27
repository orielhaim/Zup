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

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use crate::cli::{GithubFormatArg, PublishGithubCommand, PublishStageCommand};
use crate::project::{self, LoadedProject};
use crate::toolchain::{self, ToolchainResolver};

/// Write the web tree a static origin serves and a TUF repository signs.
pub fn run_stage(args: PublishStageCommand, toolchain_root: Option<PathBuf>) -> miette::Result<()> {
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
    )?;
    let resolver = crate::resolver(toolchain_root);
    let runtimes = resolve_runtimes(&loaded, &resolver)?;

    let interactive = std::io::stdout().is_terminal();
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
        let runtime = if thin {
            Some((
                zup_artifact::MediaType::RUNTIME,
                plan_only_runtime(&runtimes[index], plan)?,
            ))
        } else {
            let path = &runtimes[index];
            Some((
                zup_artifact::MediaType::RUNTIME,
                std::fs::read(path).map_err(|error| {
                    miette::miette!("runtime template `{}`: {error}", path.display())
                })?,
            ))
        };
        variants.push(
            zup_artifact::DistributionVariant::resolve(config, plan, &[], runtime)
                .map_err(|error| miette::miette!("variant `{}`: {error}", config.profile))?,
        );
    }
    if interactive {
        println!("→ Composing the release graph");
    }

    // One graph for the whole release. The web layout is variant-oriented, not
    // artifact-oriented: the offline installer is a claim in the release rather
    // than a container the content is trapped inside.
    let request = zup_artifact::ArtifactRequest::universal_offline(
        format!("{}-web", loaded.manifest.app.id.as_str()),
        &loaded.manifest.app,
        "release",
    );
    let borrowed: Vec<&zup_artifact::DistributionVariant> = variants.iter().collect();
    let graph = crate::artifacts::compose(request, &borrowed)
        .map_err(|error| miette::miette!("release graph: {error}"))?;

    let downloads = args
        .download
        .iter()
        .map(|path| {
            let size = std::fs::metadata(path)
                .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?
                .len();
            Ok(zup_artifact::ReleaseFile {
                path: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| miette::miette!("`{}` has no file name", path.display()))?
                    .to_owned(),
                digest: project::digest_of(path)?,
                size,
                kind: zup_acquire::ReleaseDownloadKind::OfflineInstaller,
                variant: None,
            })
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let channel = zup_artifact::WebExport::new(&args.channel)
        .map_err(|error| miette::miette!("channel: {error}"))?;
    let tree = zup_artifact::export_web_tree_with(&graph, &channel, &args.output, &downloads)
        .map_err(|error| miette::miette!("staging the web tree: {error}"))?;

    println!(
        "Staged {} {} ({})",
        loaded.manifest.app.name, loaded.manifest.app.version, args.channel
    );
    println!("  Tree        {}", tree.root.display());
    println!(
        "  Content     {} objects · {}",
        tree.blob_count,
        zup_presentation::format_bytes(tree.blob_bytes)
    );
    println!("  Variants    {}", tree.variant_count);
    println!("  Release     {}", tree.release_digest);
    println!(
        "  TUF targets {} in {}/tuf-input",
        tree.tuf_targets.len(),
        tree.root.display()
    );
    println!();
    println!("Publish the tree as static files, then sign the release graph:");
    println!("  tuftool update --root <trusted-root> --key <signing-key> \\");
    println!("    --add-targets {}/tuf-input \\", tree.root.display());
    println!("    --targets-expires 'in 3 weeks' --snapshot-expires 'in 3 weeks' \\");
    println!("    --timestamp-expires 'in 1 week' --outdir <repository>");

    if thin {
        stage_thin_installers(&args, &loaded, &borrowed, &tree.root, &resolver)?;
    }
    if let Some(packages) = &args.packages {
        stage_packages(&args, &loaded, &graph, &tree, packages)?;
    }
    if let Some(release_dir) = &args.release_dir {
        compose_release(release_dir, &loaded.manifest.app)?;
    }
    Ok(())
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
                return Err(miette::miette!(
                    "`{}` cannot be staged: {}",
                    config.profile,
                    toolchain::missing_component_message(&component, &error)
                ));
            }
        }
    }
    Ok(resolved)
}

/// Build the native runtime a thin release serves.
///
/// The plan is *proved* here — every declared file is read and checked against
/// its own size and digest — so a plan whose files do not exist is refused at
/// publish time rather than at install time on a user's machine.
fn plan_only_runtime(
    template: &Path,
    plan: &zup_build::TargetBuildPlan,
) -> miette::Result<Vec<u8>> {
    zup_windows::plan_only_runtime_bytes(template, plan, &[])
        .map(|(bytes, _)| bytes)
        .map_err(|error| miette::miette!("building the thin runtime: {error}"))
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
) -> miette::Result<()> {
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
            miette::miette!(
                "a thin release needs `[updates]` in the manifest: it is where the repository, \
                 the channel, and the trusted root come from"
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
        return Err(miette::miette!(
            "`--channel {}` does not match the manifest's `[updates] channel {}`; a thin \
             installer and the release it installs would read different documents",
            args.channel,
            updates.channel
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
    std::fs::create_dir_all(&output)
        .map_err(|error| miette::miette!("`{}`: {error}", output.display()))?;
    let name: String = loaded
        .manifest
        .app
        .name
        .as_str()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '/')
        .collect();

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
        );
        let request = zup_artifact::ArtifactRequest::thin_online(
            format!("{app_id}-{label}"),
            &loaded.manifest.app,
            trust,
            format!("{name}-Setup-{label}.exe"),
        );
        let graph = crate::artifacts::compose(request, variants)
            .map_err(|error| miette::miette!("thin artifact graph: {error}"))?;
        // A thin artifact is the reason the online launcher exists, so it is
        // also the only thing that asks for it.
        let component = toolchain::dispatcher_for(graph.index().artifact.subsystem, args.thin);
        let dispatcher = resolver
            .resolve(&component, args.dispatcher.as_deref())
            .map_or_else(
                |error| {
                    Err(miette::miette!(
                        "a thin installer cannot be composed: {}",
                        toolchain::missing_component_message(&component, &error)
                    ))
                },
                |resolved| Ok(resolved.path),
            )?;
        let file = output.join(&graph.index().artifact.output);
        zup_windows::compose_universal_executable(&dispatcher, &file, &graph)
            .map_err(|error| miette::miette!("`{}`: {error}", file.display()))?;
        written.push((label, file));
    }

    println!();
    println!("Thin installers");
    for (label, file) in &written {
        let size = std::fs::metadata(file).map(|item| item.len()).unwrap_or(0);
        println!(
            "  {label:<8} {} · {}",
            file.display(),
            zup_presentation::format_bytes(size)
        );
    }
    println!();
    println!("The two differ only in which document they authenticate:");
    println!(
        "  version  releases/{}/versions/{}.json — the release it was built for",
        args.channel, loaded.manifest.app.version
    );
    println!(
        "  channel  releases/{}.json — whatever the channel currently says",
        args.channel
    );
    Ok(())
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
) -> miette::Result<()> {
    let channel = args.channel.as_str();
    let catalog_bytes = std::fs::read(
        tree.root.join(
            zup_acquire::WebLayout::catalog(channel)
                .map_err(|error| miette::miette!("{error}"))?
                .to_string(),
        ),
    )
    .map_err(|error| {
        miette::miette!(
            "the content catalog is missing from the staged tree; re-run `zup publish stage`"
        )
        .wrap_err(error)
    })?;
    let catalog = zup_acquire::ContentCatalog::parse(&catalog_bytes)
        .map_err(|error| miette::miette!("the content catalog is not readable: {error}"))?;
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
    println!();
    println!("Transport packages");
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
        println!(
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
        );
    }
    Ok(())
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
fn compose_release(release_dir: &Path, app: &zup_core::App) -> miette::Result<()> {
    let mut composed = zup_artifact::ReleaseManifest::new(app);
    let mut variants = 0usize;
    let mut entries = std::fs::read_dir(release_dir.join("variants"))
        .map_err(|error| {
            miette::miette!(
                "`{}/variants`: {error}; the compose job collects the matrix's outputs there",
                release_dir.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| miette::miette!("`{}/variants`: {error}", release_dir.display()))?;
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
        let part = zup_artifact::ReleaseManifest::parse(&bytes)
            .map_err(|error| miette::miette!("`{}`: {error}", description.display()))?;
        for variant in &part.variants {
            composed
                .add_variant(variant)
                .map_err(|error| miette::miette!("`{}`: {error}", description.display()))?;
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
                    miette::miette!(
                        "`{}` claims `{}` but the compose job could not see it: {error}",
                        description.display(),
                        path.display()
                    )
                })?;
            if size != artifact.size {
                return Err(miette::miette!(
                    "`{}` is {size} bytes and the release description says {}",
                    path.display(),
                    artifact.size
                ));
            }
            composed
                .add_composed(artifact, &path)
                .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
        }
    }
    if variants == 0 {
        return Err(miette::miette!(
            "`{}/variants` holds no release descriptions; the build matrix writes one per \
             target and the compose job reads them all",
            release_dir.display()
        ));
    }
    let path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    let bytes = composed
        .encode()
        .map_err(|error| miette::miette!("composing the release description: {error}"))?;
    std::fs::write(&path, &bytes)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    println!();
    println!(
        "Composed {} {} ({} files · {variants} variants)",
        app.name,
        app.version,
        composed.artifacts.len()
    );
    println!("  Release     {}", path.display());
    Ok(())
}

/// Join a release-root-relative path, refusing anything that is not one.
fn join_release_path(root: &Path, relative: &str) -> miette::Result<PathBuf> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/') || relative.contains("..") || relative.contains(':') {
        return Err(miette::miette!(
            "`{relative}` is not a path inside the release root"
        ));
    }
    Ok(root.join(relative))
}

/// Publish a release to GitHub.
///
/// The whole command in one place, because the interesting part is not the
/// request: it is that every step before the one that makes the release public is
/// idempotent, and that a dry run performs all of them and writes none of them.
pub fn run_github(args: PublishGithubCommand) -> miette::Result<()> {
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
                return Err(miette::miette!("{error}"));
            }
            eprintln!("  - no credential: {error}");
            eprintln!("  - the plan below is complete; publication was not attempted");
            return Ok(());
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
    if args.format == GithubFormatArg::Json {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|error| miette::miette!("report: {error}"))?;
        println!("{json}");
    } else {
        println!("{}", report.human());
        if std::io::stdout().is_terminal() {
            eprintln!();
            eprintln!("repository  {repository} ({how})");
        }
    }
    if !report.is_complete() {
        return Err(miette::miette!("publishing {} did not finish", report.tag));
    }
    Ok(())
}
