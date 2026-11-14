use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use zup_artifact::{
    ArtifactGraph, DistributionVariant, MediaType, ReleaseManifest, VariantSources,
};
use zup_automation::{AutomationResult, ByteCount, Details, PublishStageDetails};

use super::{inventory, measure};

pub(super) fn run(args: crate::cli::PublishStageCommand) -> miette::Result<AutomationResult> {
    if args.thin || !args.download.is_empty() {
        return Err(miette::miette!(
            "matrix composition reads complete installer artifacts; stage thin launchers and external downloads separately"
        ));
    }
    let root = args.release_dir.as_ref().expect("matrix release directory");
    let project = crate::project::load_manifest(&args.manifest)?;
    let reporter = crate::failure::Reporter::new(args.format);
    let path = super::compose_release(root, &project.app, &reporter)?;
    let mut release = super::read_release(root)?;
    let expected = super::matrix(&project)?
        .into_iter()
        .map(|v| (v.profile, v.target))
        .collect::<BTreeSet<_>>();
    let actual = release
        .variants
        .iter()
        .map(|v| (v.id.clone(), v.target))
        .collect();
    if expected != actual {
        return Err(miette::miette!(
            "matrix artifacts do not cover the project's exact target profiles"
        ));
    }
    let spool = tempfile::tempdir().map_err(|e| miette::miette!("composition spool: {e}"))?;
    let mut variants = BTreeMap::new();
    for artifact in &release.artifacts {
        let source = inventory::file(root, &artifact.path)?;
        for variant in import(&source, artifact, spool.path())? {
            if variant.application() != &release.application {
                return Err(miette::miette!("matrix artifact application mismatch"));
            }
            if !release
                .variants
                .iter()
                .any(|v| v.id == variant.id() && v.target == *variant.target())
            {
                return Err(miette::miette!(
                    "matrix installer target differs from its release manifest"
                ));
            }
            if let Some(previous) = variants.insert(variant.id().to_owned(), variant.clone())
                && previous != variant
            {
                return Err(miette::miette!("conflicting matrix variants"));
            }
        }
    }
    let request = zup_artifact::ArtifactRequest::universal_offline(
        format!("{}-web", release.application.id),
        &release.application,
        "release",
    );
    let borrowed = variants.values().collect::<Vec<_>>();
    let graph = crate::artifacts::compose(request, &borrowed)
        .map_err(|e| miette::miette!("matrix composition: {e}"))?;
    let channel =
        zup_artifact::WebExport::new(&args.channel).map_err(|e| miette::miette!("{e}"))?;
    let tree = zup_artifact::export_web_tree(&graph, &channel, &args.output)
        .map_err(|e| miette::miette!("{e}"))?;
    natives(&mut release, &graph, root)?;
    let packages = match &args.packages {
        Some(output) => super::stage_packages(
            &args,
            &release.application,
            &graph,
            &tree,
            output,
            &reporter,
        )?,
        None => Vec::new(),
    };
    for relative in &tree.tuf_targets {
        let (size, digest) = measure(&inventory::file(&tree.root, relative)?)?;
        release.documents.push(zup_artifact::ReleaseDocument {
            path: relative.clone(),
            identity: zup_artifact::BuildArtifact { digest, size },
        });
    }
    if let Some(directory) = &args.packages {
        for package in &packages {
            let descriptor = zup_publish::document_name(&zup_publish::DocumentKind::Package {
                variant: &package.variant,
            });
            let (size, digest) = measure(&inventory::file(directory, &descriptor)?)?;
            release.transport.push(zup_artifact::ReleaseTransport {
                variant: package.variant.clone(),
                descriptor,
                identity: zup_artifact::BuildArtifact { digest, size },
            });
        }
        inventory::packages(&release, directory, Some(&tree.root))?;
    }
    release.validate().map_err(|e| miette::miette!("{e}"))?;
    zup_core::publish(
        &path,
        &release.encode().map_err(|e| miette::miette!("{e}"))?,
    )
    .map_err(|e| miette::miette!("{e}"))?;
    let signing = crate::build::signing_plan(&release, root, None)?;
    zup_core::publish(
        &root.join(zup_signing::SIGNING_PLAN_NAME),
        &signing.encode().map_err(|e| miette::miette!("{e}"))?,
    )
    .map_err(|e| miette::miette!("{e}"))?;
    Ok(
        AutomationResult::new(zup_automation::OPERATION_PUBLISH_STAGE)
            .with_application(crate::automation::application(&release.application))
            .with_release_manifest(crate::automation::project_path(&path))
            .with_artifacts(crate::automation::artifacts(&release))
            .with_details(Details::PublishStage(PublishStageDetails {
                web_root: crate::automation::project_path(&tree.root),
                channel: args.channel,
                objects: super::count(tree.blob_count),
                object_bytes: ByteCount::new(tree.blob_bytes),
                variants: super::count(tree.variant_count),
                tuf_inputs: super::count_of(&tree.tuf_targets),
                release_digest: Some(tree.release_digest.to_hex()),
                packages,
                thin_installers: Vec::new(),
                release_manifest: Some(crate::automation::project_path(&path)),
            }))
            .with_summary(format!(
                "Composed {} verified matrix variants",
                release.variants.len()
            )),
    )
}

fn import(
    path: &Path,
    product: &zup_artifact::ReleaseArtifact,
    spool: &Path,
) -> miette::Result<Vec<DistributionVariant>> {
    if product.kind == zup_artifact::ArtifactKind::Universal {
        let artifact = zup_windows::UniversalArtifact::open(path)
            .map_err(|e| miette::miette!("matrix artifact: {e}"))?;
        use zup_artifact::ContentSource as _;
        let mut variants = Vec::new();
        for id in &product.variants {
            let manifest = artifact
                .view()
                .verify_variant(id)
                .map_err(|e| miette::miette!("{e}"))?;
            let mut sources = VariantSources::new();
            for digest in manifest.content_digests() {
                let entry = artifact
                    .view()
                    .table()
                    .entry(&digest)
                    .ok_or_else(|| miette::miette!("missing matrix content"))?;
                let bytes = artifact
                    .view()
                    .store()
                    .blob(entry)
                    .map_err(|e| miette::miette!("{e}"))?;
                let target = spool.join(digest.to_hex());
                std::fs::write(&target, bytes).map_err(|e| miette::miette!("{e}"))?;
                sources = sources.with_file(digest, target);
            }
            for descriptor in [manifest.runtime, manifest.preset].into_iter().flatten() {
                let bytes = artifact
                    .view()
                    .read(&descriptor)
                    .map_err(|e| miette::miette!("{e}"))?;
                sources = sources.with_bytes(descriptor.digest, bytes);
            }
            variants.push(
                DistributionVariant::from_manifest(id, manifest, sources)
                    .map_err(|e| miette::miette!("{e}"))?,
            );
        }
        return Ok(variants);
    }
    if product.variants.len() != 1 {
        return Err(miette::miette!(
            "a single installer must name exactly one matrix variant"
        ));
    }
    let opened = zup_compose::open(path).map_err(|e| miette::miette!("matrix installer: {e}"))?;
    let package = &opened.package;
    let mut sources = VariantSources::new();
    for digest in package.required_digests() {
        let target = spool.join(digest.to_hex());
        let mut output = std::fs::File::create(&target).map_err(|e| miette::miette!("{e}"))?;
        package
            .copy_verified_blob(&digest, &mut output)
            .map_err(|e| miette::miette!("{e}"))?;
        sources = sources.with_file(digest, target);
    }
    let runtime = if package.plan().installer.target.is_windows() {
        let bytes = zup_bundle::BundleWriter::encode_plan_only_plan(
            package.plan(),
            &package.plan().plugins,
        )
        .map_err(|e| miette::miette!("{e}"))?;
        let output = spool.join(format!("{}-runtime.exe", product.variants[0]));
        zup_compose::compose_windows(path, &output, &bytes, None)
            .map_err(|e| miette::miette!("{e}"))?;
        std::fs::read(output).map_err(|e| miette::miette!("{e}"))?
    } else {
        zup_compose::linux_runtime_bytes(path).map_err(|e| miette::miette!("{e}"))?
    };
    let mut natives = vec![(MediaType::Runtime, runtime)];
    if let Some(preset) = opened.preset {
        natives.push((MediaType::Preset, preset));
    }
    Ok(vec![
        DistributionVariant::from_package(&product.variants[0], package, sources, &natives)
            .map_err(|e| miette::miette!("{e}"))?,
    ])
}

fn natives(
    release: &mut ReleaseManifest,
    graph: &ArtifactGraph,
    root: &Path,
) -> miette::Result<()> {
    let mut objects = BTreeMap::new();
    for image in graph.runtimes().iter().chain(graph.presets()) {
        let descriptor = image.descriptor;
        let object =
            objects
                .entry(descriptor.digest)
                .or_insert_with(|| zup_artifact::ReleaseObject {
                    path: zup_acquire::blob_path(&descriptor.digest).to_string(),
                    identity: zup_artifact::BuildArtifact {
                        digest: descriptor.digest,
                        size: descriptor.size,
                    },
                    media_type: descriptor.media_type,
                    variants: Vec::new(),
                });
        object.variants.push(image.id.clone());
        if descriptor.media_type == MediaType::Runtime {
            let target = release
                .variants
                .iter()
                .find(|v| v.id == image.id)
                .ok_or_else(|| miette::miette!("foreign runtime variant"))?
                .target;
            let directory = root.join("runtime");
            std::fs::create_dir_all(&directory).map_err(|e| miette::miette!("{e}"))?;
            std::fs::write(
                directory.join(format!("{}{}", image.id, target.executable_suffix())),
                &image.bytes,
            )
            .map_err(|e| miette::miette!("{e}"))?;
        }
    }
    release.objects = objects.into_values().collect();
    Ok(())
}
