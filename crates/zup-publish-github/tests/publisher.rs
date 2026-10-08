mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{Behaviour, Github, fill};
use zup_publish::{
    Application, ProductClass, ProductRole, PublishReport, ReleasePlan, ReleaseProduct,
    SourceClaim, TagIntent,
};
use zup_publish_github::{PublishRequest, publish, supplied};

fn plan(assets: &[(&str, &[u8])]) -> (ReleasePlan, BTreeMap<String, PathBuf>) {
    let mut plan = ReleasePlan::new(
        Application::new(
            zup_core::AppId::new("com.acme.app").expect("a valid app id"),
            "Acme",
            "1.4.0",
        ),
        TagIntent::required("v1.4.0"),
    )
    .with_source(SourceClaim::none());
    let mut sources = BTreeMap::new();
    let root = std::env::temp_dir().join(format!(
        "zup-publish-test-{}-{}",
        std::process::id(),
        assets.len()
    ));
    std::fs::create_dir_all(&root).expect("the staging directory is created");
    for (index, (name, bytes)) in assets.iter().enumerate() {
        let path = root.join(format!("{index}-{name}"));
        std::fs::write(&path, bytes).expect("the asset is written");
        plan.push(ReleaseProduct::new(
            *name,
            ProductRole::Install,
            ProductClass::UserFacing,
            zup_core::hash_bytes(bytes),
            bytes.len() as u64,
        ));
        sources.insert((*name).to_owned(), path);
    }
    (plan, sources)
}

fn request(plan: ReleasePlan, sources: BTreeMap<String, PathBuf>) -> PublishRequest {
    PublishRequest {
        sources,
        dry_run: false,
        ..PublishRequest::new(plan)
    }
}

fn against(github: &Github, request: &PublishRequest) -> PublishRequest {
    PublishRequest {
        endpoints: Some(zup_publish_github::ClientEndpoints {
            api: github.api_origin(),
            upload: github.upload_origin(),
        }),
        ..request.clone()
    }
}

async fn run(github: &Github, request: &PublishRequest) -> PublishReport {
    publish(
        &github.repository(),
        &supplied("test-token"),
        &against(github, request),
    )
    .await
    .expect("the publication completes")
}

fn labels(report: &PublishReport, phase: &str) -> Vec<String> {
    report
        .phases
        .iter()
        .find(|entry| entry.name == phase)
        .map(|entry| entry.steps.iter().map(|step| step.label.clone()).collect())
        .unwrap_or_default()
}

fn assets() -> Vec<(&'static str, Vec<u8>)> {
    let mut installer = vec![0u8; 4096];
    fill(&mut installer, 1);
    let mut manifest = br#"{"schema":1}"#.to_vec();
    fill(&mut manifest, 2);
    vec![
        ("Acme-Windows-Setup.exe", installer),
        ("zup-release.json", manifest),
    ]
}

#[tokio::test]
async fn a_first_publication_creates_a_draft_uploads_and_publishes_once() {
    let github = Github::start(Behaviour::absent());
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(plan, sources)).await;

    assert!(report.is_complete(), "{}", report.human());
    let uploaded = labels(&report, "Uploading");
    assert!(
        uploaded.contains(&"Acme-Windows-Setup.exe 4096".to_owned())
            || uploaded
                .iter()
                .any(|label| label.starts_with("Acme-Windows-Setup.exe")),
        "{uploaded:?}"
    );
    assert!(
        labels(&report, "Verifying")
            .iter()
            .any(|label| label.contains("SHA-256")),
        "{:?}",
        labels(&report, "Verifying")
    );
    assert!(
        labels(&report, "Publishing")
            .iter()
            .any(|label| label.contains("v1.4.0 published"))
    );
    assert_eq!(github.asset_names().len(), 2);
    let receipt = report.receipt.expect("a receipt");
    assert_eq!(receipt.state.as_str(), "published");
    assert_eq!(receipt.products.len(), 2);
    for product in &receipt.products {
        let local = staged
            .iter()
            .find(|(name, _)| *name == product.name)
            .expect("a staged asset");
        assert_eq!(product.digest, zup_core::hash_bytes(&local.1));
    }
}

#[tokio::test]
async fn a_second_publication_uploads_nothing_and_reports_no_change() {
    let github = Github::start(Behaviour::absent());
    let staged = assets();
    let first = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    run(&github, &request(first.0, first.1)).await;
    let attempts_after_first = github.upload_attempts();

    let second = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(second.0, second.1)).await;

    assert!(report.is_complete(), "{}", report.human());
    assert_eq!(
        github.upload_attempts(),
        attempts_after_first,
        "a matching asset is never re-uploaded"
    );
    let uploading = labels(&report, "Uploading");
    assert!(
        uploading
            .iter()
            .all(|label| label.contains("already uploaded")),
        "{uploading:?}"
    );
    assert_eq!(
        report.receipt.expect("a receipt").state.as_str(),
        "unchanged"
    );
}

#[tokio::test]
async fn a_resumed_draft_uploads_only_what_is_missing() {
    let github = Github::start(Behaviour::new());
    let staged = assets();
    let installer = &staged[0].1;
    github.put("Acme-Windows-Setup.exe", installer);

    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(plan, sources)).await;

    assert!(report.is_complete(), "{}", report.human());
    assert_eq!(
        github.upload_attempts(),
        1,
        "only the missing asset is sent"
    );
    let uploading = labels(&report, "Uploading");
    assert!(
        uploading
            .iter()
            .any(|label| label.contains("already uploaded"))
    );
}

#[tokio::test]
async fn a_differing_asset_on_a_draft_is_refused_not_overwritten() {
    let github = Github::start(Behaviour::new());
    github.put("Acme-Windows-Setup.exe", b"a different installer");
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );

    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a conflict is a refusal");
    assert!(error.to_string().contains("and the host holds"), "{error}");
    assert_eq!(
        github.asset_names().len(),
        1,
        "the existing asset is left alone"
    );
    let behaviour = github.behaviour();
    assert!(behaviour.draft, "the release is still a draft");
}

#[tokio::test]
async fn a_differing_asset_is_replaced_only_when_asked_and_only_on_a_draft() {
    let github = Github::start(Behaviour::new());
    github.put("Acme-Windows-Setup.exe", b"a different installer");
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let mut publish_request = request(plan, sources);
    publish_request.replace_conflicts = true;
    let report = run(&github, &publish_request).await;

    assert!(report.is_complete(), "{}", report.human());
    let assets = github.assets();
    let installer = assets
        .iter()
        .find(|(name, ..)| name == "Acme-Windows-Setup.exe")
        .expect("the asset");
    assert_eq!(installer.1 as usize, staged[0].1.len());
}

#[tokio::test]
async fn a_published_release_with_a_matching_plan_is_a_successful_no_op() {
    let github = Github::start(Behaviour::published());
    let staged = assets();
    for (name, bytes) in &staged {
        github.put(name, bytes);
    }
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(plan, sources)).await;

    assert!(report.is_complete(), "{}", report.human());
    assert_eq!(
        report.receipt.expect("a receipt").state.as_str(),
        "unchanged"
    );
    assert_eq!(github.upload_attempts(), 0);
}

#[tokio::test]
async fn a_published_release_with_a_different_plan_fails_loudly() {
    let github = Github::start(Behaviour::published());
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a published release is not a retry target");
    assert!(error.to_string().contains("already published"), "{error}");
}

#[tokio::test]
async fn a_failed_upload_that_leaves_a_starter_asset_is_cleaned_up_and_retried() {
    let mut behaviour = Behaviour::new();
    behaviour.upload_failures = 1;
    behaviour.leaves_starter = true;
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(plan, sources)).await;

    assert!(report.is_complete(), "{}", report.human());
    assert!(github.upload_attempts() >= 2, "the upload was retried");
    let assets = github.assets();
    assert_eq!(
        assets.len(),
        2,
        "the starter remnant was removed, not left beside the real asset: {assets:?}"
    );
    for (name, size, _) in &assets {
        assert!(*size > 0, "`{name}` is still empty");
    }
}

#[tokio::test]
async fn a_wrong_remote_digest_removes_the_asset_and_refuses() {
    let mut behaviour = Behaviour::new();
    behaviour.report_wrong_digest = true;
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a mismatched digest is a refusal, not a publication");
    assert!(error.to_string().contains("sha256:"), "{error}");
    assert!(
        github.asset_names().is_empty(),
        "the mismatched asset is removed rather than left for someone to download"
    );
}

#[tokio::test]
async fn a_renamed_asset_is_refused_rather_than_published_under_another_name() {
    let mut behaviour = Behaviour::new();
    behaviour.rename_to = Some("renamed-by-the-host.exe".to_owned());
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a host that renames an asset breaks publication identity");
    assert!(
        error.to_string().contains("renamed-by-the-host.exe"),
        "{error}"
    );
}

#[tokio::test]
async fn a_rate_limit_is_waited_out_rather_than_treated_as_a_refusal() {
    let mut behaviour = Behaviour::new();
    behaviour.rate_limited = true;
    behaviour.status = Some(403);
    behaviour.retry_after = Some(0);
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a persistent rate limit is still a failure");
    assert!(error.to_string().contains("rate limit"), "{error}");
    assert!(
        github.requests() > 1,
        "the rate limit was retried rather than refused outright"
    );
}

#[tokio::test]
async fn a_rejected_credential_says_so() {
    let mut behaviour = Behaviour::new();
    behaviour.status = Some(401);
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("bad-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a rejected token is a failure");
    let text = error.to_string();
    assert!(
        text.contains("credential") || text.contains("Bad credentials"),
        "{text}"
    );
    assert!(
        !text.contains("bad-token"),
        "a token never reaches a diagnostic"
    );
}

#[tokio::test]
async fn a_dry_run_creates_nothing_and_says_so() {
    let github = Github::start(Behaviour::absent());
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let mut publish_request = request(plan, sources);
    publish_request.dry_run = true;
    let report = run(&github, &publish_request).await;

    assert!(report.is_complete(), "{}", report.human());
    assert!(report.dry_run);
    assert_eq!(report.receipt.expect("a receipt").state.as_str(), "planned");
    assert!(github.asset_names().is_empty());
    let log = github.log();
    assert!(
        !log.iter()
            .any(|line| line.contains("POST") && line.contains("assets")),
        "a dry run sends no upload: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|line| line.contains("POST /repos") && line.ends_with("releases")),
        "a dry run creates no draft: {log:?}"
    );
}

#[tokio::test]
async fn a_private_repository_is_reported_because_it_cannot_host_a_thin_installer() {
    let mut behaviour = Behaviour::new();
    behaviour.private = true;
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let report = run(&github, &request(plan, sources)).await;

    let preparing = report
        .phases
        .iter()
        .find(|phase| phase.name == "Preparing release")
        .expect("a preparing phase");
    let warning = preparing
        .steps
        .iter()
        .find(|step| step.label == "private repository")
        .expect("a private-repository step");
    assert_eq!(warning.status.as_str(), "warn");
    assert!(
        warning
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("credential")),
        "{:?}",
        warning.detail
    );
}

#[tokio::test]
async fn an_archived_repository_is_refused_before_a_draft_exists() {
    let mut behaviour = Behaviour::absent();
    behaviour.archived = true;
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("an archived repository cannot take a release");
    assert!(error.to_string().contains("archived"), "{error}");
    let log = github.log();
    assert!(
        !log.iter()
            .any(|line| line.contains("releases") && line.starts_with("POST")),
        "no draft was created: {log:?}"
    );
}

#[tokio::test]
async fn immutability_is_reported_when_the_host_answers_and_not_invented_when_it_does_not() {
    for (reported, expected) in [(Some(true), "ok"), (Some(false), "warn"), (None, "info")] {
        let mut behaviour = Behaviour::new();
        behaviour.release_immutable = reported;
        let github = Github::start(behaviour);
        let staged = assets();
        let (plan, sources) = plan(
            &staged
                .iter()
                .map(|(n, b)| (*n, b.as_slice()))
                .collect::<Vec<_>>(),
        );
        let report = run(&github, &request(plan, sources)).await;
        let notice = report
            .notices
            .iter()
            .find(|notice| notice.label == "github immutable release")
            .unwrap_or_else(|| panic!("an immutability notice for {reported:?}"));
        assert_eq!(notice.status, expected, "{reported:?}");
        if expected == "warn" {
            assert!(
                notice
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("Settings")),
                "a disabled recommendation says where to enable it: {:?}",
                notice.detail
            );
        }
    }
}

#[tokio::test]
async fn the_receipt_carries_the_providers_identifiers_and_no_secret() {
    let github = Github::start(Behaviour::absent());
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let mut publish_request = request(plan, sources);
    let receipt_path = std::env::temp_dir().join(format!("receipt-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&receipt_path);
    publish_request.receipt = Some(receipt_path.clone());
    run(&github, &publish_request).await;

    let bytes = std::fs::read(&receipt_path).expect("the receipt was written");
    let text = String::from_utf8(bytes).expect("the receipt is text");
    assert!(
        !text.contains("test-token"),
        "a receipt never carries a token"
    );
    let receipt: zup_publish_github::GithubReceipt =
        serde_json::from_str(&text).expect("the receipt parses");
    assert_eq!(receipt.host, "github.com");
    assert_eq!(receipt.repository, "acme/acme");
    assert_eq!(receipt.tag, "v1.4.0");
    assert!(receipt.release_id > 0);
    assert!(receipt.assets.iter().all(|asset| asset.id > 0));
    let _ = std::fs::remove_file(&receipt_path);
}

#[tokio::test]
async fn a_duplicate_filename_is_a_refusal_not_a_retry() {
    let mut behaviour = Behaviour::new();
    behaviour.status = Some(422);
    let github = Github::start(behaviour);
    let staged = assets();
    let (plan, sources) = plan(
        &staged
            .iter()
            .map(|(n, b)| (*n, b.as_slice()))
            .collect::<Vec<_>>(),
    );
    let error = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await
    .expect_err("a 422 is a refusal");
    assert!(error.to_string().contains("422"), "{error}");
    assert!(error.to_string().contains("Validation Failed"), "{error}");
}
