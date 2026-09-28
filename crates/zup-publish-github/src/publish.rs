//! The publication state machine.
//!
//! # The shape of it
//!
//! ```text
//! resolve/verify tag
//!     ↓
//! create or find draft          ← resumable: a draft is a place to resume to
//!     ↓
//! upload every required asset   ← resumable: a matching asset is skipped
//!     ↓
//! verify every remote asset     ← name, size, SHA-256, state
//!     ↓
//! verify release plan completeness
//!     ↓
//! publish the draft once
//! ```
//!
//! Every step is idempotent, and that is the whole reason this is a sequence of small
//! functions rather than one big one: `zup publish github` is expected to be run again
//! after a failure, on purpose or by a CI retry, and the second run has to be
//! indistinguishable from a first run that had nothing to do.
//!
//! # What is never done
//!
//! - A public release is never created before its files exist. `create_draft` asks for
//!   a draft unconditionally, and the only call that clears the flag is the last one.
//! - A failed upload does not delete the draft. A draft with eleven of twelve files is
//!   a *resumable* draft; deleting it throws away eleven uploads so that the twelfth can
//!   start again.
//! - A published release is never mutated. A mismatch against a published release is a
//!   refusal, not a repair: the tag is already public, and the bytes behind it are
//!   already what somebody downloaded.
//! - A conflicting asset on a draft is never silently overwritten. It is a refusal
//!   unless the caller explicitly asked to replace it, because "different bytes under
//!   this name" means either the plan changed or something else wrote here, and neither
//!   is the publisher's call.
//!
//! # Why uploads are reconciled rather than trusted
//!
//! GitHub documents a specific and unpleasant failure: an upload can fail upstream
//! *after* the asset record was created, leaving a zero-byte asset in the `starter`
//! state under the name that was being uploaded. That asset then blocks every
//! subsequent upload of the same name, because the name is taken.
//!
//! So an uncertain upload is never a bare retry. The sequence is: list the release's
//! assets, look for a `starter` entry with this name, remove exactly that one, and
//! only then retry - bounded, with backoff. A *valid* asset is never removed, including
//! when the HTTP request that uploaded it failed after the server processed it, because
//! the reconciliation is exactly what tells the two apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use zup_publish::{
    AssetAction, AssetState, Notice, ProductRole, ProductState, PublicationState, ReleasePlan,
    ReleaseProduct, RemoteAsset, ReportBuilder, StepStatus, classify, describe_conflict,
};

use crate::api::{Asset, GithubClient, Release};
use crate::error::GithubError;
use crate::notes::{self, DownloadRow, NotesPolicy};
use crate::receipt::{GithubAsset, GithubReceipt};
use crate::repository::GithubRepository;
use crate::token::Token;

/// How many times one asset upload is attempted.
///
/// Small, because a retry of an upload restarts the whole transfer rather than
/// resuming it: GitHub's asset API has no resumable upload, so attempt five of a
/// two gigabyte installer is five gigabytes sent to learn the same thing twice.
const MAX_UPLOAD_ATTEMPTS: u32 = 3;

/// How one publication is asked for.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub plan: ReleasePlan,
    /// Where each product's bytes are on this machine.
    pub sources: BTreeMap<String, PathBuf>,
    /// The notes policy.
    pub notes: NotesPolicy,
    /// Text for [`NotesPolicy::Text`].
    pub notes_text: Option<String>,
    /// A file to read for [`NotesPolicy::File`].
    pub notes_file: Option<PathBuf>,
    /// Whether to leave the release a draft.
    pub draft: bool,
    /// Whether to mark the release a prerelease.
    pub prerelease: bool,
    /// Whether to plan and verify without writing anything.
    pub dry_run: bool,
    /// Whether a differing asset on a *draft* may be removed and re-uploaded.
    pub replace_conflicts: bool,
    /// Where to write the receipt, if anywhere.
    pub receipt: Option<PathBuf>,
    /// Where the client sends its requests, when that is not the installation's
    /// own bases. See [ClientEndpoints](crate::ClientEndpoints).
    pub endpoints: Option<crate::api::ClientEndpoints>,
}

impl PublishRequest {
    /// A request for `plan`, with nothing filled in yet.
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

    /// Say where a product's bytes are.
    pub fn with_source(mut self, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.sources.insert(name.into(), path.into());
        self
    }

    /// The file for one product, if it was staged.
    pub fn source_of(&self, name: &str) -> Option<&Path> {
        self.sources.get(name).map(PathBuf::as_path)
    }
}

/// The `POST`/`PATCH` body for a release.
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
    /// A request to create a draft for `tag`.
    pub fn draft(tag: &str, prerelease: bool) -> Self {
        Self {
            tag_name: Some(tag.to_owned()),
            draft: true,
            prerelease,
            ..Self::default()
        }
    }

    /// The request that publishes a draft, carrying only what changed.
    ///
    /// `draft: false` is the point of the whole sequence. Everything else here is
    /// absent unless the caller set it, because a `PATCH` with an empty `name`
    /// clears a title somebody wrote.
    pub fn publish(prerelease: bool) -> Self {
        Self {
            draft: false,
            prerelease,
            ..Self::default()
        }
    }
}

/// Run one publication.
pub async fn publish(
    repository: &GithubRepository,
    token: &Token,
    request: &PublishRequest,
) -> Result<zup_publish::PublishReport, GithubError> {
    let plan = &request.plan;
    let products = plan.preflight(&crate::api::LIMITS)?;
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

    // ---------------------------------------------------------- preparing
    report.phase("Preparing release");
    prepare(&client, plan, request, &products, &mut report).await?;

    report.phase("Resolving release");
    let release = locate(&client, plan, request, &mut report).await?;
    receipt.release_id = release.id;
    receipt.url = Some(release.url().to_owned());
    receipt
        .target_commitish
        .clone_from(&release.target_commitish);
    let was_published = !release.draft;

    // ------------------------------------------------------------ uploading
    report.phase("Uploading");
    let by_name: BTreeMap<String, Asset> = if request.dry_run {
        report.info(
            "remote assets",
            "a dry run does not read the release either",
        );
        BTreeMap::new()
    } else {
        client
            .assets(release.id)
            .await?
            .into_iter()
            .map(|asset| (asset.name.clone(), asset))
            .collect()
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

    // ----------------------------------------------------------- verifying
    report.phase("Verifying");
    if request.dry_run {
        report.skip();
    } else {
        verify(&client, &release, &products, &mut report).await?;
    }

    // ---------------------------------------------------------- publishing
    report.phase("Publishing");
    let mut final_release = release.clone();
    if request.dry_run {
        report.info(
            "nothing to publish",
            "a dry run stops before the release goes live",
        );
        receipt.state = PublicationState::Planned;
    } else if was_published {
        // Nothing was written: either every asset was already present, or the
        // comparison refused. Both mean the release already carries this plan.
        receipt.state = PublicationState::Unchanged;
        report.ok(format!("{} already published", release.tag_name));
    } else if request.draft {
        receipt.state = PublicationState::Draft;
        report.info("left as a draft", "the caller asked for a draft release");
    } else {
        final_release = client
            .publish(release.id, &CreateRelease::publish(request.prerelease))
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

    // ------------------------------------------------------------- receipt
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
        // The receipt is what a re-run reads to decide whether the release is
        // already published, so a torn write is a publisher that cannot resume:
        // written atomically and flushed, like every other durable document here.
        write_receipt(path, &bytes)?;
        report.phase("Receipt");
        report.ok(path.display().to_string());
    }
    report.receipt(receipt.to_publish_receipt());
    Ok(report.finish())
}

/// Write the receipt where a crash cannot leave half of one.
///
/// A temp sibling, a flush, and a rename. Not because a receipt is precious - it
/// is a report - but because a half-written receipt is *worse* than none: a
/// re-run parses it to decide whether the release is already published, and a
/// truncated one either refuses a resumable draft or, read loosely, claims the
/// release went live when it did not.
fn write_receipt(path: &Path, bytes: &[u8]) -> Result<(), GithubError> {
    let mut temporary = path.to_path_buf();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "github-publish.json".to_owned());
    temporary.set_file_name(format!(".{name}.partial"));
    let _ = std::fs::remove_file(&temporary);
    let written = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(GithubError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        });
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(GithubError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        });
    }
    Ok(())
}

/// Check the plan's own claims and the host's limits, before anything is written.
async fn prepare(
    client: &GithubClient,
    plan: &ReleasePlan,
    request: &PublishRequest,
    products: &[ReleaseProduct],
    report: &mut ReportBuilder,
) -> Result<(), GithubError> {
    if let Some(commit) = &plan.source.commit {
        report.ok(format!("commit {commit}"));
    }
    report.ok(format!("{} assets validated", products.len()));
    if let Some((_name, size)) = plan.largest() {
        report.ok(format!("largest asset {size} bytes"));
    }
    // A repository has to be reachable before a draft exists. A 404 here is a
    // typo in `owner/name`; a 403 is a token that cannot write to it. Both are
    // cheap to learn now and expensive to learn after a release exists.
    let info = client.repository_info().await.inspect_err(|error| {
        report.failed("repository", error.to_string());
    })?;
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

/// Find the release this publication targets, creating a draft if there is none.
///
/// A dry run stops here. It has established the plan, the limits, the
/// credential, and the repository; it does not manufacture a release so that it
/// has something to report about.
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
            create.generate_release_notes =
                matches!(request.notes, NotesPolicy::Generated).then_some(true);
            let release = client.create_draft(&create).await?;
            report.ok(format!("draft {} created", plan.tag.tag));
            Ok(release)
        }
    }
}

/// A release that does not exist yet, for a dry run.
///
/// Everything downstream checks `dry_run` before touching a release, so the only
/// thing this has to be is something that compiles and reads sensibly in a
/// report.
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

/// Upload what is missing, and reconcile what a failed attempt left behind.
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
    if !release.draft {
        // A published release is not a retry target, in either direction. A
        // *different* file under a planned name is the obvious case; adding a
        // name that was never there is the same mistake in a different hat. The
        // tag is already public, and everything under it is already what somebody
        // may have downloaded.
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

/// How many steps have failed so far, so a caller can tell whether it just
/// added one.
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

/// One asset upload, with reconciliation and a bounded retry.
///
/// The reconciliation is the point. An upload that fails ambiguously may have
/// succeeded, so the next thing is never "send it again" - it is "ask what the
/// host holds under this name". A `starter` entry is a failed remnant and is
/// removed; anything else is left alone, because a valid asset that a retry
/// clobbered would be worse than the original failure.
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
        match client
            .upload_asset(release.id, &product.name, path, product.size)
            .await
        {
            Ok(asset) => {
                // A successful response is not proof. The digest is.
                if let Some(remote) = asset.sha256()
                    && remote != product.digest
                {
                    // The bytes that arrived are not the bytes that were signed.
                    // The asset is removed and the upload refused, because leaving
                    // a mismatched file on a draft is how a broken release becomes
                    // a published one.
                    let _ = client.delete_asset(asset.id).await;
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
                    let _ = client.delete_asset(asset.id).await;
                    return Err(GithubError::Size {
                        name: product.name.clone(),
                        expected: product.size,
                        found: asset.size,
                    });
                }
                if asset.name != product.name {
                    // The host renamed it. A release manifest that points at the
                    // planned name would be pointing at nothing.
                    let _ = client.delete_asset(asset.id).await;
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
                // Reconcile before every decision, including giving up, so the
                // draft is left in a state a rerun can reason about.
                let removed = reconcile(client, release, &product.name, report).await?;
                if !retryable || attempt >= MAX_UPLOAD_ATTEMPTS {
                    if !removed {
                        report.failed(format!("{} not uploaded", product.name), error.to_string());
                    }
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

/// Ask what the host holds under `name`, and remove only a failed remnant.
///
/// Returns whether a remnant was removed, so the caller can tell "the upload
/// failed and left nothing behind" from "the upload failed and I cleaned up
/// after it" - which are different situations for the next run.
async fn reconcile(
    client: &GithubClient,
    release: &Release,
    name: &str,
    report: &mut ReportBuilder,
) -> Result<bool, GithubError> {
    let assets = client.assets(release.id).await?;
    let Some(asset) = assets.into_iter().find(|asset| asset.name == name) else {
        return Ok(false);
    };
    let remote: RemoteAsset = asset.remote();
    if remote.is_starter() {
        report.info(
            format!("{} removing a failed upload", name),
            "an empty asset was left behind and its name is taken",
        );
        client.delete_asset(asset.id).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Prove that every planned file is on the host, by the host's own digest.
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
                // A host that reports no digest cannot be checked against. Saying
                // so is the honest answer; treating it as a match is the one thing
                // that would make this phase decorative.
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

/// Record the integrity facts GitHub reported.
///
/// The distinction that matters: an artifact attestation answers "which workflow
/// built these bytes", and an immutable-release attestation answers "are these
/// the exact assets published as this tag". Both are worth having, neither is
/// proof that the software is safe, and a publisher that reports only one of them
/// is implying the other.
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
            // An Enterprise Server too old to report the field is not the same as
            // a github.com repository that has it off, and saying "not enabled"
            // for the first would be a claim zup cannot support.
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
        download_url: Some(asset.browser_download_url.clone()),
    }
}

/// Compose the notes body, if the policy writes one.
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
