use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use zup_publish::{
    AssetAction, AssetState, Notice, ProductRole, ProductState, PublicationState, ReleasePlan,
    ReleaseProduct, ReportBuilder, StepStatus, classify, describe_conflict,
};

use crate::api::{Asset, GithubClient, Release};
use crate::error::GithubError;
use crate::notes::{self, DownloadRow, NotesPolicy};
use crate::receipt::{GithubAsset, GithubReceipt};
use crate::repository::GithubRepository;
use crate::token::Token;

const MAX_UPLOAD_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub plan: ReleasePlan,
    pub sources: BTreeMap<String, PathBuf>,
    pub notes: NotesPolicy,
    pub notes_text: Option<String>,
    pub notes_file: Option<PathBuf>,
    pub draft: bool,
    pub prerelease: bool,
    pub dry_run: bool,
    pub replace_conflicts: bool,
    pub receipt: Option<PathBuf>,
    pub endpoints: Option<crate::api::ClientEndpoints>,
}

impl PublishRequest {
    pub fn new(plan: ReleasePlan) -> Self {
        Self {
            plan,
            sources: BTreeMap::new(),
            notes: NotesPolicy::default(),
            notes_text: None,
            notes_file: None,
            draft: false,
            prerelease: false,
            dry_run: false,
            replace_conflicts: false,
            receipt: None,
            endpoints: None,
        }
    }

    pub fn with_source(mut self, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.sources.insert(name.into(), path.into());
        self
    }

    pub fn source_of(&self, name: &str) -> Option<&Path> {
        self.sources.get(name).map(PathBuf::as_path)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateRelease {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate_release_notes: Option<bool>,
}

impl CreateRelease {
    pub fn draft(tag: &str, prerelease: bool) -> Self {
        Self {
            tag_name: Some(tag.to_owned()),
            draft: true,
            prerelease,
            ..Self::default()
        }
    }

    pub fn publish(prerelease: bool) -> Self {
        Self {
            draft: false,
            prerelease,
            ..Self::default()
        }
    }
}

pub async fn publish(
    repository: &GithubRepository,
    token: &Token,
    request: &PublishRequest,
) -> Result<zup_publish::PublishReport, GithubError> {
    let plan = &request.plan;
    let products = plan.preflight(&crate::limits::LIMITS)?;
    let (_snapshot, verified) = snapshot_sources(request, &products)?;
    let request = &verified;
    let plan = &request.plan;
    let client = match &request.endpoints {
        Some(endpoints) => GithubClient::at(repository, token, endpoints)?,
        None => GithubClient::new(repository, token)?,
    };

    let mut report = ReportBuilder::new(
        "github",
        repository.to_string(),
        plan.tag.tag.clone(),
        plan.application.version.clone(),
    )
    .dry_run(request.dry_run);
    report.plan(products.len(), plan.bytes());
    let mut receipt = GithubReceipt::new(repository, &plan.tag.tag, 0);

    report.phase("Preparing release");
    prepare(&client, plan, request, &products, &mut report).await?;
    if let Some(path) = &request.receipt
        && path.exists()
    {
        let previous = GithubReceipt::parse(
            &std::fs::read(path).map_err(|e| source_error(&format!("receipt: {e}")))?,
        )
        .map_err(|e| source_error(&e))?;
        if previous.host != repository.host.host
            || previous.repository != repository.to_string()
            || previous.tag != plan.tag.tag
            || previous.claimed_commit != plan.source.commit
            || previous.source.as_ref().is_some_and(|source| {
                plan.source
                    .commit
                    .as_ref()
                    .is_some_and(|commit| &source.commit != commit)
            })
        {
            return Err(source_error(
                "previous receipt names a different source, repository, or tag; inspect it before retrying",
            ));
        }
        if let Some(source) = &previous.source {
            check_source(&client, plan, Some(source), false).await?;
        }
    }
    let resolved = resolve_source(&client, plan, request.dry_run).await?;
    receipt.source = resolved.clone();
    receipt.claimed_commit.clone_from(&plan.source.commit);
    receipt.notices.push(Notice::info("build source", if plan.source.commit.is_some() {
        "recorded checkout commit matches the remote tag; this is not source signature authentication"
    } else { "no build commit was recorded; the resolved tag is not proof of the artifact's build source" }));

    report.phase("Resolving release");
    let release = locate(&client, plan, request, &mut report).await?;
    if release.tag_name != plan.tag.tag {
        return Err(source_error("release returned a different tag"));
    }
    if release
        .target_commitish
        .as_deref()
        .filter(|sha| zup_publish::valid_commit(sha))
        .is_some_and(|sha| resolved.as_ref().is_some_and(|source| sha != source.commit))
    {
        return Err(source_error(
            "release commit metadata contradicts the resolved tag",
        ));
    }
    check_source(&client, plan, resolved.as_ref(), request.dry_run).await?;
    receipt.release_id = release.id;
    receipt.url = Some(release.url().to_owned());
    receipt
        .target_commitish
        .clone_from(&release.target_commitish);
    let was_published = !release.draft;

    report.phase("Uploading");
    let by_name: BTreeMap<String, Asset> = if request.dry_run {
        report.info(
            "remote assets",
            "a dry run does not read the release either",
        );
        BTreeMap::new()
    } else {
        let assets = client.assets(release.id).await?;
        let mut names = BTreeMap::new();
        for asset in assets {
            if !products.iter().any(|product| product.name == asset.name)
                || names.insert(asset.name.clone(), asset).is_some()
            {
                return Err(source_error(
                    "remote release contains extra or duplicate assets; inspect the draft before retrying",
                ));
            }
        }
        names
    };
    upload(
        &client,
        &release,
        request,
        &products,
        &by_name,
        &mut receipt,
        &mut report,
    )
    .await?;

    report.phase("Verifying");
    if request.dry_run {
        report.skip();
    } else {
        verify(&client, &release, &products, &mut report).await?;
    }

    report.phase("Publishing");
    check_source(&client, plan, resolved.as_ref(), request.dry_run).await?;
    let mut final_release = release.clone();
    if request.dry_run {
        report.info(
            "nothing to publish",
            "a dry run stops before the release goes live",
        );
        receipt.state = PublicationState::Planned;
    } else if was_published {
        receipt.state = PublicationState::Unchanged;
        report.ok(format!("{} already published", release.tag_name));
    } else if request.draft {
        receipt.state = PublicationState::Draft;
        report.info("left as a draft", "the caller asked for a draft release");
    } else {
        final_release = client
            .update(release.id, &CreateRelease::publish(request.prerelease))
            .await?;
        receipt.state = PublicationState::Published;
        report.ok(format!("{} published", release.tag_name));
        if final_release
            .body
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            report.warn(
                "release notes",
                "this release has no body; set `[publish.github] notes = \"file\"` or pass \
                 --notes-text to write one",
            );
        }
    }
    integrity(&final_release, &mut receipt, &mut report);
    if final_release.id != release.id || final_release.tag_name != plan.tag.tag {
        return Err(source_error(
            "publication acknowledgment names a different release",
        ));
    }
    check_source(&client, plan, resolved.as_ref(), request.dry_run).await?;

    if let Some(path) = &request.receipt {
        let bytes = receipt
            .encode()
            .map_err(|reason| GithubError::Decode { reason })?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| GithubError::Io {
                path: parent.display().to_string(),
                reason: error.to_string(),
            })?;
        }
        write_receipt(path, &bytes)?;
        report.phase("Receipt");
        report.ok(path.display().to_string());
    }
    report.receipt(receipt.to_publish_receipt());
    Ok(report.finish())
}

fn write_receipt(path: &Path, bytes: &[u8]) -> Result<(), GithubError> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let io = |error: std::io::Error| GithubError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
    temporary.write_all(bytes).map_err(io)?;
    temporary.as_file().sync_all().map_err(io)?;
    temporary.persist(path).map_err(|error| io(error.error))?;
    Ok(())
}

fn source_error(reason: &str) -> GithubError {
    GithubError::Source {
        reason: reason.to_owned(),
    }
}

async fn resolve_source(
    client: &GithubClient,
    plan: &ReleasePlan,
    dry_run: bool,
) -> Result<Option<crate::api::ResolvedTag>, GithubError> {
    match client.tag(&plan.tag.tag).await? {
        Some(resolved) => {
            if plan
                .source
                .commit
                .as_ref()
                .is_some_and(|expected| expected != &resolved.commit)
            {
                return Err(source_error(
                    "remote tag differs from the build commit; use the correct tag or rebuild",
                ));
            }
            Ok(Some(resolved))
        }
        None if plan.tag.create => {
            let commit = plan
                .source
                .commit
                .as_deref()
                .ok_or_else(|| source_error("creating a tag requires a proven build commit"))?;
            client.prove_commit(commit).await?;
            if dry_run {
                Ok(None)
            } else {
                client.create_tag(&plan.tag.tag, commit).await.map(Some)
            }
        }
        None => Err(GithubError::MissingTag {
            tag: plan.tag.tag.clone(),
        }),
    }
}

async fn check_source(
    client: &GithubClient,
    plan: &ReleasePlan,
    resolved: Option<&crate::api::ResolvedTag>,
    dry_run: bool,
) -> Result<(), GithubError> {
    if dry_run && resolved.is_none() {
        return Ok(());
    }
    if client.tag(&plan.tag.tag).await?.as_ref() != resolved {
        return Err(source_error(
            "tag moved during publication; inspect the remote ref and rebuild before retrying",
        ));
    }
    Ok(())
}

fn verify_local(path: &Path, product: &ReleaseProduct) -> Result<(), GithubError> {
    let file = std::fs::File::open(path).map_err(|e| GithubError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let (size, digest) =
        zup_core::hash_reader(std::io::BufReader::new(file)).map_err(|e| GithubError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
    if size != product.size {
        return Err(GithubError::Size {
            name: product.name.clone(),
            expected: product.size,
            found: size,
        });
    }
    if digest != product.digest {
        return Err(GithubError::Digest {
            name: product.name.clone(),
            expected: product.digest.to_hex(),
            found: digest.to_hex(),
        });
    }
    Ok(())
}

fn snapshot_sources(
    request: &PublishRequest,
    products: &[ReleaseProduct],
) -> Result<(tempfile::TempDir, PublishRequest), GithubError> {
    let root =
        tempfile::tempdir().map_err(|e| source_error(&format!("cannot snapshot release: {e}")))?;
    let mut verified = request.clone();
    verified.sources.clear();
    for product in products {
        let original = staged(request, product)?;
        let metadata = std::fs::symlink_metadata(original)
            .map_err(|e| source_error(&format!("staged file: {e}")))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(source_error("staged assets must be regular files"));
        }
        let destination = root.path().join(&product.name);
        std::fs::copy(original, &destination)
            .map_err(|e| source_error(&format!("snapshot: {e}")))?;
        verify_local(&destination, product)?;
        verified.sources.insert(product.name.clone(), destination);
    }
    if request.sources.len() != verified.sources.len() {
        return Err(source_error(
            "staged files differ from the finite plan inventory",
        ));
    }
    Ok((root, verified))
}

async fn prepare(
    client: &GithubClient,
    plan: &ReleasePlan,
    request: &PublishRequest,
    products: &[ReleaseProduct],
    report: &mut ReportBuilder,
) -> Result<(), GithubError> {
    for product in products {
        verify_local(staged(request, product)?, product)?;
    }
    if let Some(commit) = &plan.source.commit {
        report.ok(format!("commit {commit}"));
    }
    report.ok(format!("{} assets validated", products.len()));
    if let Some((_name, size)) = plan.largest() {
        report.ok(format!("largest asset {size} bytes"));
    }
    let info = client.repository_info().await.inspect_err(|error| {
        report.failed("repository", error.to_string());
    })?;
    if !info.full_name.eq_ignore_ascii_case(client.repository()) {
        return Err(source_error("GitHub returned a different repository"));
    }
    if info.archived {
        return Err(GithubError::DraftState {
            state: "archived".to_owned(),
            reason: format!(
                "{} is archived, and a release cannot be published to it",
                info.full_name
            ),
        });
    }
    report.ok(format!("repository {}", info.full_name));
    if info.private {
        report.warn(
            "private repository",
            "release assets need a credential to fetch, so GitHub cannot be a thin \
             installer's content host for this project",
        );
    } else {
        report.ok("public repository");
    }
    if request.dry_run {
        report.info("no mutation", "a dry run creates and uploads nothing");
    }
    Ok(())
}

async fn locate(
    client: &GithubClient,
    plan: &ReleasePlan,
    request: &PublishRequest,
    report: &mut ReportBuilder,
) -> Result<Release, GithubError> {
    match client.release_by_tag(&plan.tag.tag).await? {
        Some(release) => {
            report.ok(if release.draft {
                format!("draft {} resumed", plan.tag.tag)
            } else {
                format!("{} already published", plan.tag.tag)
            });
            Ok(release)
        }
        None if request.dry_run => {
            report.ok(format!("no release for {} yet", plan.tag.tag));
            Ok(placeholder(&plan.tag.tag))
        }
        None => {
            let mut create = CreateRelease::draft(&plan.tag.tag, request.prerelease);
            create.target_commitish = Some(
                client
                    .tag(&plan.tag.tag)
                    .await?
                    .ok_or_else(|| GithubError::MissingTag {
                        tag: plan.tag.tag.clone(),
                    })?
                    .commit,
            );
            create.generate_release_notes =
                matches!(request.notes, NotesPolicy::Generated).then_some(true);
            let release = client.create_draft(&create).await?;
            report.ok(format!("draft {} created", plan.tag.tag));
            Ok(release)
        }
    }
}

fn placeholder(tag: &str) -> Release {
    Release {
        id: 0,
        tag_name: tag.to_owned(),
        name: None,
        body: None,
        draft: true,
        prerelease: false,
        immutable: None,
        has_attestation: None,
        html_url: String::new(),
        target_commitish: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload(
    client: &GithubClient,
    release: &Release,
    request: &PublishRequest,
    products: &[ReleaseProduct],
    by_name: &BTreeMap<String, Asset>,
    receipt: &mut GithubReceipt,
    report: &mut ReportBuilder,
) -> Result<(), GithubError> {
    if request.dry_run {
        report.info("nothing to upload", "a dry run sends no bytes");
        return Ok(());
    }
    for product in products {
        if let AssetAction::Conflict(_) | AssetAction::Replace(_) = classify(
            product,
            by_name.get(&product.name).map(Asset::remote).as_ref(),
        ) && (!request.replace_conflicts || !release.draft)
        {
            return Err(GithubError::PublishedConflict {
                tag: release.tag_name.clone(),
            });
        }
    }
    if !release.draft {
        let before = report_failures(report);
        for product in products {
            let remote = by_name.get(&product.name);
            match classify(product, remote.map(Asset::remote).as_ref()) {
                AssetAction::Present => {}
                AssetAction::Conflict(conflict) => {
                    report.failed(
                        format!("{} differs", product.name),
                        describe_conflict(&conflict),
                    );
                }
                _ => {
                    report.failed(
                        format!("{} is missing", product.name),
                        format!(
                            "`{}` is already published and does not carry this file. A published \
                             release is not a retry target: bump the version, or delete the release \
                             by hand.",
                            release.tag_name
                        ),
                    );
                }
            }
        }
        if report_failures(report) > before {
            return Err(GithubError::PublishedConflict {
                tag: release.tag_name.clone(),
            });
        }
    }
    for product in products {
        let remote = by_name.get(&product.name);
        match classify(product, remote.map(Asset::remote).as_ref()) {
            AssetAction::Present => {
                let asset = remote.expect("a present asset came from the remote");
                report.ok(format!("{} already uploaded", product.name));
                receipt.assets.push(record(
                    asset,
                    product,
                    if request.dry_run {
                        ProductState::Planned
                    } else {
                        ProductState::AlreadyPresent
                    },
                ));
            }
            AssetAction::Upload => {
                let path = staged(request, product)?;
                let asset = upload_one(client, release, product, path, report).await?;
                receipt
                    .assets
                    .push(record(&asset, product, ProductState::Uploaded));
            }
            AssetAction::Replace(conflict) => {
                if !request.replace_conflicts || !release.draft {
                    return Err(source_error(
                        "draft asset replacement requires replace_conflicts",
                    ));
                }
                let asset = remote.expect("a replacement came from the remote");
                report.warn(
                    format!("{} re-uploading", product.name),
                    describe_conflict(&conflict),
                );
                client.delete_asset(asset.id).await?;
                let path = staged(request, product)?;
                let asset = upload_one(client, release, product, path, report).await?;
                receipt
                    .assets
                    .push(record(&asset, product, ProductState::Replaced));
            }
            AssetAction::Conflict(conflict) => {
                if !request.replace_conflicts || !release.draft {
                    report.failed(
                        format!("{} differs", product.name),
                        describe_conflict(&conflict),
                    );
                    return Err(GithubError::Status {
                        status: 422,
                        message: Some(describe_conflict(&conflict)),
                    });
                }
                let asset = remote.expect("a conflict came from the remote");
                report.warn(
                    format!("{} replacing", product.name),
                    describe_conflict(&conflict),
                );
                client.delete_asset(asset.id).await?;
                let path = staged(request, product)?;
                let asset = upload_one(client, release, product, path, report).await?;
                receipt
                    .assets
                    .push(record(&asset, product, ProductState::Replaced));
            }
        }
    }
    Ok(())
}

fn report_failures(report: &ReportBuilder) -> usize {
    report
        .phases()
        .iter()
        .flat_map(|phase| phase.steps.iter())
        .filter(|step| step.status == StepStatus::Fail)
        .count()
}

fn staged<'a>(
    request: &'a PublishRequest,
    product: &ReleaseProduct,
) -> Result<&'a Path, GithubError> {
    request
        .source_of(&product.name)
        .ok_or_else(|| GithubError::Io {
            path: product.name.clone(),
            reason: "the release plan names this file but no file was staged for it".to_owned(),
        })
}

async fn upload_one(
    client: &GithubClient,
    release: &Release,
    product: &ReleaseProduct,
    path: &Path,
    report: &mut ReportBuilder,
) -> Result<Asset, GithubError> {
    let mut attempt = 1u32;
    let mut delay = Duration::from_millis(750);
    loop {
        verify_local(path, product)?;
        match client
            .upload_asset(release.id, &product.name, path, product.size)
            .await
        {
            Ok(asset) => {
                if let Some(remote) = asset.sha256()
                    && remote != product.digest
                {
                    report.failed(
                        format!("{} digest mismatch", product.name),
                        format!("expected sha256:{}", product.digest.to_hex()),
                    );
                    return Err(GithubError::Digest {
                        name: product.name.clone(),
                        expected: product.digest.to_hex(),
                        found: remote.to_hex(),
                    });
                }
                if asset.size != product.size {
                    return Err(GithubError::Size {
                        name: product.name.clone(),
                        expected: product.size,
                        found: asset.size,
                    });
                }
                if asset.name != product.name {
                    return Err(GithubError::Renamed {
                        expected: product.name.clone(),
                        found: asset.name.clone(),
                    });
                }
                report.sized(product.name.clone(), product.size);
                return Ok(asset);
            }
            Err(error) => {
                let retryable = matches!(
                    error,
                    GithubError::Status {
                        status: 500 | 502 | 503 | 504,
                        ..
                    } | GithubError::RateLimited { .. }
                        | GithubError::Transport { .. }
                );
                let found = client
                    .assets(release.id)
                    .await?
                    .into_iter()
                    .filter(|a| a.name == product.name)
                    .collect::<Vec<_>>();
                if !found.is_empty() {
                    if found.len() == 1
                        && matches!(
                            classify(product, Some(&found[0].remote())),
                            AssetAction::Present
                        )
                    {
                        return Ok(found[0].clone());
                    }
                    return Err(source_error(
                        "upload outcome is ambiguous or conflicting; inspect the draft and explicitly authorize replacement before retrying",
                    ));
                }
                if !retryable || attempt >= MAX_UPLOAD_ATTEMPTS {
                    report.failed(format!("{} not uploaded", product.name), error.to_string());
                    return Err(error);
                }
                report.info(
                    format!("{} retrying", product.name),
                    format!("attempt {attempt} of {MAX_UPLOAD_ATTEMPTS}"),
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(20));
                attempt += 1;
            }
        }
    }
}

async fn verify(
    client: &GithubClient,
    release: &Release,
    products: &[ReleaseProduct],
    report: &mut ReportBuilder,
) -> Result<(), GithubError> {
    let assets = client.assets(release.id).await?;
    let by_name: BTreeMap<&str, &Asset> = assets
        .iter()
        .map(|asset| (asset.name.as_str(), asset))
        .collect();
    if assets.len() != products.len() || by_name.len() != assets.len() {
        return Err(source_error(
            "final remote asset inventory differs from the finite release plan",
        ));
    }
    let mut compared = 0usize;
    for product in products {
        let Some(asset) = by_name.get(product.name.as_str()) else {
            report.failed(
                format!("{} missing", product.name),
                "the release does not carry this file",
            );
            return Err(GithubError::NotFound {
                what: format!("asset `{}` on the release", product.name),
            });
        };
        if asset.size != product.size {
            report.failed(
                format!("{} wrong size", product.name),
                format!(
                    "expected {} bytes, github reports {}",
                    product.size, asset.size
                ),
            );
            return Err(GithubError::Size {
                name: product.name.clone(),
                expected: product.size,
                found: asset.size,
            });
        }
        if asset.asset_state() == AssetState::Starter {
            report.failed(
                format!("{} not uploaded", product.name),
                "github still lists this as an empty starter asset",
            );
            return Err(GithubError::DraftState {
                state: "starter".to_owned(),
                reason: format!("`{}` is an empty asset from a failed upload", product.name),
            });
        }
        match asset.sha256() {
            Some(remote) if remote == product.digest => compared += 1,
            Some(remote) => {
                report.failed(
                    format!("{} digest mismatch", product.name),
                    format!("expected sha256:{}", product.digest.to_hex()),
                );
                return Err(GithubError::Digest {
                    name: product.name.clone(),
                    expected: product.digest.to_hex(),
                    found: remote.to_hex(),
                });
            }
            None => {
                report.warn(
                    format!("{} digest unavailable", product.name),
                    "github reports no sha256 for this asset, so its bytes were not compared",
                );
            }
        }
    }
    if compared == products.len() {
        report.ok(format!(
            "all {} github SHA-256 digests match",
            products.len()
        ));
    } else {
        report.warn(
            format!("{compared} of {} digests compared", products.len()),
            "github did not report a digest for the rest",
        );
    }
    Ok(())
}

fn integrity(release: &Release, receipt: &mut GithubReceipt, report: &mut ReportBuilder) {
    receipt.immutable = release.immutable;
    receipt.attestation = release.has_attestation;
    match release.immutable {
        Some(true) => {
            receipt.notices.push(Notice::ok("github immutable release"));
            report.notice(Notice::ok("github immutable release"));
            report.notice(match release.has_attestation {
                Some(true) => Notice::ok("release attestation available"),
                _ => Notice::info("release attestation", "not reported by this GitHub host"),
            });
        }
        Some(false) => {
            receipt
                .notices
                .push(Notice::warn("github immutable release", "not enabled"));
            report.notice(Notice::warn(
                "github immutable release",
                "not enabled - Settings → Releases → Enable release immutability",
            ));
        }
        None => {
            receipt.notices.push(Notice::info(
                "github immutable release",
                "not reported by this host",
            ));
            report.notice(Notice::info(
                "github immutable release",
                "not reported by this GitHub host",
            ));
        }
    }
}

fn record(asset: &Asset, product: &ReleaseProduct, state: ProductState) -> GithubAsset {
    GithubAsset {
        id: asset.id,
        name: asset.name.clone(),
        size: asset.size,
        state,
        digest: product.digest,
        remote_digest: asset.sha256(),
        download_url: Some(asset.browser_download_url.clone()),
    }
}

pub fn compose_notes(
    request: &PublishRequest,
    generated: Option<String>,
    products: &[ReleaseProduct],
) -> Option<String> {
    let file_body = request
        .notes_file
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let rows: Vec<DownloadRow> = products
        .iter()
        .filter(|product| product.role == ProductRole::Install)
        .map(|product| DownloadRow {
            name: product.name.clone(),
            size: product.size,
            role: product.role.as_str().to_owned(),
        })
        .collect();
    notes::compose(
        &request.notes,
        generated,
        file_body,
        request.notes_text.clone(),
        &rows,
    )
}
