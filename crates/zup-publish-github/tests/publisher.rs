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
    let root = tempfile::tempdir().unwrap().keep();
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
async fn incomplete_product_dependency_is_rejected_before_contact() {
    let github = Github::start(Behaviour::absent());
    let (mut plan, sources) = plan(&[("descriptor.json", b"descriptor")]);
    plan.install_artifacts[0]
        .requires
        .push("missing.zup.part0001".into());
    let result = publish(
        &github.repository(),
        &supplied("test-token"),
        &against(&github, &request(plan, sources)),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(github.requests(), 0);
}

#[tokio::test]
async fn foreign_tag_winning_creation_race_prevents_release_mutation() {
    let github = Github::start(Behaviour {
        tag_missing: true,
        tag_creation_collision: true,
        ..Behaviour::absent()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    plan.tag.create = true;
    assert!(
        publish(
            &github.repository(),
            &supplied("test-token"),
            &against(&github, &request(plan, sources))
        )
        .await
        .is_err()
    );
    assert_eq!(github.upload_attempts(), 0);
    assert!(
        github
            .log()
            .iter()
            .filter(|s| !s.starts_with("GET "))
            .all(|s| s.contains("/git/refs"))
    );
}

#[tokio::test]
async fn source_mismatch_is_rejected_before_mutation() {
    let github = Github::start(Behaviour {
        tag_commit: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
        ..Behaviour::absent()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert_eq!(github.upload_attempts(), 0);
    assert!(github.log().iter().all(|entry| entry.starts_with("GET ")));
}

#[rstest::rstest]
#[case(false, false, 0, false, false)]
#[case(false, false, 1, false, false)]
#[case(false, false, 2, false, false)]
#[case(true, false, 0, false, true)]
#[case(true, true, 0, false, false)]
#[case(true, true, 0, true, false)]
#[tokio::test]
async fn exact_tag_resolution_and_opted_in_creation(
    #[case] missing: bool,
    #[case] create: bool,
    #[case] depth: u8,
    #[case] lost_ack: bool,
    #[case] rejected: bool,
) {
    let github = Github::start(Behaviour {
        tag_missing: missing,
        annotated_depth: depth,
        lost_tag_ack: lost_ack,
        ..Behaviour::absent()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    plan.tag.create = create;
    let root = tempfile::tempdir().unwrap();
    let mut request = against(&github, &request(plan, sources));
    request.receipt = Some(root.path().join("receipt.json"));
    let result = publish(&github.repository(), &supplied("test-token"), &request).await;
    assert_eq!(result.is_err(), rejected, "{result:?}");
    if rejected {
        assert!(github.log().iter().all(|s| s.starts_with("GET ")));
    } else {
        let receipt = zup_publish_github::GithubReceipt::parse(
            &std::fs::read(request.receipt.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt.source.unwrap().commit,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let uploads = github.upload_attempts();
        assert!(
            publish(&github.repository(), &supplied("test-token"), &request)
                .await
                .unwrap()
                .is_complete()
        );
        assert_eq!(github.upload_attempts(), uploads);
    }
}

#[rstest::rstest]
#[case("short")]
#[case("gggggggggggggggggggggggggggggggggggggggg")]
#[case("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")]
#[tokio::test]
async fn malformed_source_never_contacts_github(#[case] commit: &str) {
    let github = Github::start(Behaviour::absent());
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of(commit);
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert_eq!(github.requests(), 0);
}

#[rstest::rstest]
#[case(true, None)]
#[case(false, Some(1))]
#[tokio::test]
async fn wrong_repository_or_moved_ref_fails_closed(
    #[case] wrong_repository: bool,
    #[case] moved_after_reads: Option<usize>,
) {
    let github = Github::start(Behaviour {
        wrong_repository,
        moved_after_reads,
        ..Behaviour::new()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert_eq!(github.upload_attempts(), 0);
    assert!(github.log().iter().all(|s| s.starts_with("GET ")));
}

#[tokio::test]
async fn corrupted_local_product_fails_before_remote_access() {
    let github = Github::start(Behaviour::absent());
    let (plan, sources) = plan(&[("setup.exe", b"installer")]);
    std::fs::write(&sources["setup.exe"], b"corrupted").unwrap();
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert_eq!(github.requests(), 0);
}

#[rstest::rstest]
#[case("refs/heads/main", None, None, None)]
#[case("", Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), None, None)]
#[case("", None, Some("foreign-release"), None)]
#[case("", None, None, Some("malformed-sha"))]
#[tokio::test]
async fn contradictory_remote_source_evidence_is_refused(
    #[case] reference: &str,
    #[case] target_commitish: Option<&str>,
    #[case] release_tag: Option<&str>,
    #[case] tag_commit: Option<&str>,
) {
    let github = Github::start(Behaviour {
        ref_override: (!reference.is_empty()).then(|| reference.into()),
        target_commitish: target_commitish.map(str::to_owned),
        release_tag: release_tag.map(str::to_owned),
        tag_commit: tag_commit.map(str::to_owned),
        ..Behaviour::new()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert!(github.log().iter().all(|s| s.starts_with("GET ")));
}

#[rstest::rstest]
#[case("release/v1.4.0+rc.1")]
#[case("release;$()")]
#[tokio::test]
async fn legitimate_tag_punctuation_is_encoded_and_resolved_exactly(#[case] tag: &str) {
    let github = Github::start(Behaviour {
        release_tag: Some(tag.into()),
        ..Behaviour::new()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.tag.tag = tag.into();
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(run(&github, &request(plan, sources)).await.is_complete());
}

#[rstest::rstest]
#[case("../foreign")]
#[case("release..bad")]
#[case("ref.lock")]
#[case("tag\nforged-output=true")]
#[case("tag^{commit}")]
#[case("tag name")]
#[tokio::test]
async fn invalid_tag_syntax_never_reaches_the_api(#[case] tag: &str) {
    let github = Github::start(Behaviour::absent());
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.tag.tag = tag.into();
    let request = against(&github, &request(plan, sources));
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert_eq!(github.requests(), 0);
}

#[tokio::test]
async fn conflicting_receipt_prevents_even_opted_in_tag_creation() {
    let github = Github::start(Behaviour {
        tag_missing: true,
        ..Behaviour::absent()
    });
    let (mut plan, sources) = plan(&[("setup.exe", b"installer")]);
    plan.source = SourceClaim::of("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    plan.tag.create = true;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("receipt.json");
    let mut previous = zup_publish_github::GithubReceipt::new(&github.repository(), "v1.4.0", 4242);
    previous.claimed_commit = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
    std::fs::write(&path, previous.encode().unwrap()).unwrap();
    let mut request = against(&github, &request(plan, sources));
    request.receipt = Some(path);
    assert!(
        publish(&github.repository(), &supplied("test-token"), &request)
            .await
            .is_err()
    );
    assert!(github.log().iter().all(|s| s.starts_with("GET ")));
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
    assert!(matches!(
        error,
        zup_publish_github::GithubError::PublishedConflict { .. }
    ));
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
async fn a_failed_upload_requires_explicit_draft_replacement_on_resume() {
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
    let mut request = request(plan, sources);
    assert!(
        publish(
            &github.repository(),
            &supplied("test-token"),
            &against(&github, &request)
        )
        .await
        .is_err()
    );
    assert!(github.log().iter().all(|s| !s.starts_with("DELETE ")));
    request.replace_conflicts = true;
    let report = run(&github, &request).await;

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
async fn a_wrong_remote_digest_retains_the_draft_for_inspection_and_refuses() {
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
        github.asset_names().len() == 1 && github.behaviour().draft,
        "a mismatched acknowledgment leaves an unpublished draft for inspection"
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

#[tokio::test]
async fn api_and_uploads_authorize_their_origins_and_encode_urls_once() {
    let api = Github::start(Behaviour::new());
    let uploads = Github::start(Behaviour::new());
    let client = zup_publish_github::GithubClient::at(
        &api.repository(),
        &supplied("test-token"),
        &zup_publish_github::ClientEndpoints {
            api: api.api_origin(),
            upload: uploads.upload_origin(),
        },
    )
    .expect("a client");
    client
        .release_by_tag("release/v1 +%20")
        .await
        .expect("a release");
    client.assets(4242).await.expect("assets");
    let asset = tempfile::NamedTempFile::new().expect("a file");
    std::fs::write(asset.path(), b"payload").expect("write");
    client
        .upload_asset(4242, "asset +.bin", asset.path(), 7)
        .await
        .expect("upload");
    assert_eq!(
        api.authorization(),
        vec![Some("Bearer test-token".to_owned()); 2]
    );
    assert_eq!(
        uploads.authorization(),
        vec![Some("Bearer test-token".to_owned())]
    );
    assert_eq!(
        api.targets(),
        [
            "/repos/acme/acme/releases/tags/release%2Fv1%20+%2520",
            "/repos/acme/acme/releases/4242/assets?per_page=100&page=1",
        ]
    );
    assert_eq!(uploads.asset_names(), ["asset +.bin"]);
}
