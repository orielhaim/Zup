//! The publication state machine, against a host that misbehaves.
//!
//! These are the tests the whole crate exists for. Everything GitHub documents
//! about how a release upload can fail — and a few things it does not document but
//! reliably does — are simulated here, and each one has an assertion about what
//! the publisher did about it.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{Behaviour, Github, fill};
use zup_publish::{
    Application, ProductClass, ProductRole, PublishReport, ReleasePlan, ReleaseProduct,
    SourceClaim, TagIntent,
};
use zup_publish_github::{GithubClient, PublishRequest, publish, supplied};

/// A plan with one installer and one manifest, so a test can vary the assets
/// without restating the whole shape.
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

/// A request pointed at a mock host.
///
/// The endpoint seam is the same one an organisation with an egress proxy uses;
/// a test just happens to be the case where the address is a loopback port.
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
    // Every product's receipt digest is the local one, which is the whole point
    // of verifying before publishing.
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

#[tokio::test]
async fn the_client_speaks_one_api_version_and_binds_the_credential() {
    let github = Github::start(Behaviour::new());
    let _ = GithubClient::at(
        &github.repository(),
        &supplied("test-token"),
        &zup_publish_github::ClientEndpoints {
            api: github.api_origin(),
            upload: github.upload_origin(),
        },
    )
    .expect("a client")
    .repository_info()
    .await;
    let log = github.log();
    assert!(log.iter().any(|line| line.contains("acme/acme")), "{log:?}");
}

/// A repository is discovered from a remote, and two candidates are a refusal.
mod discovery {
    use std::collections::BTreeMap;
    use zup_publish_github::{parse_remote_url, parse_remotes, resolve};

    struct Env(BTreeMap<String, String>);

    impl zup_publish_github::Environment for Env {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    #[test]
    fn every_common_remote_form_reduces_to_the_same_three_strings() {
        for url in [
            "https://github.com/acme/acme.git",
            "git@github.com:acme/acme.git",
            "ssh://git@github.com/acme/acme.git",
            "https://user@github.com/acme/acme",
        ] {
            let parsed = parse_remote_url(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(
                parsed,
                ("github.com".into(), "acme".into(), "acme".into()),
                "{url}"
            );
        }
    }

    #[test]
    fn a_non_github_remote_is_not_a_candidate() {
        assert!(parse_remote_url("https://gitlab.com/acme/acme.git").is_none());
        assert!(parse_remote_url("https://git.acme.internal/acme/acme.git").is_none());
    }

    #[test]
    fn remotes_are_read_out_of_a_git_config_in_file_order() {
        let config = "\
[core]
\trepositoryformatversion = 0
[remote \"upstream\"]
\turl = https://github.com/acme/upstream.git
\tfetch = +refs/heads/*:refs/remotes/upstream/*
[remote \"origin\"]
\turl = git@github.com:acme/acme.git
";
        assert_eq!(
            parse_remotes(config),
            vec![
                (
                    "upstream".to_owned(),
                    "https://github.com/acme/upstream.git".to_owned()
                ),
                (
                    "origin".to_owned(),
                    "git@github.com:acme/acme.git".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn the_environment_wins_over_a_remote() {
        let root = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("a .git directory");
        std::fs::write(
            root.path().join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:from-remote/acme.git\n",
        )
        .expect("the config is written");
        let environment = Env(BTreeMap::from([(
            "GITHUB_REPOSITORY".to_owned(),
            "from-env/acme".to_owned(),
        )]));
        let resolved = resolve(None, &environment, root.path()).expect("a repository");
        assert_eq!(resolved.repository.to_string(), "from-env/acme");
        assert_eq!(resolved.discovery.as_str(), "github_repository");
    }

    #[test]
    fn a_single_github_remote_is_the_answer() {
        let root = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("a .git directory");
        std::fs::write(
            root.path().join(".git/config"),
            "[remote \"fork\"]\n\turl = git@github.com:acme/acme.git\n",
        )
        .expect("the config is written");
        let resolved = resolve(None, &Env(BTreeMap::new()), root.path()).expect("a repository");
        assert_eq!(resolved.repository.to_string(), "acme/acme");
        assert_eq!(resolved.remote.as_deref(), Some("fork"));
    }

    #[test]
    fn two_github_remotes_and_no_origin_is_an_actionable_refusal() {
        let root = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("a .git directory");
        std::fs::write(
            root.path().join(".git/config"),
            "[remote \"fork\"]\n\turl = git@github.com:acme/fork.git\n\
             [remote \"mirror\"]\n\turl = https://github.com/acme/acme.git\n",
        )
        .expect("the config is written");
        let error = resolve(None, &Env(BTreeMap::new()), root.path())
            .expect_err("two candidates and no origin is a question, not a guess");
        let text = error.to_string();
        assert!(text.contains("--repo"), "{text}");
        assert!(text.contains("fork"), "{text}");
    }

    #[test]
    fn an_explicit_repository_beats_everything() {
        let environment = Env(BTreeMap::from([(
            "GITHUB_REPOSITORY".to_owned(),
            "from-env/acme".to_owned(),
        )]));
        let spec = zup_publish_github::RepositorySpec::parse("explicit/acme").expect("a spec");
        let resolved =
            resolve(Some(&spec), &environment, std::path::Path::new(".")).expect("a repository");
        assert_eq!(resolved.repository.to_string(), "explicit/acme");
    }

    #[test]
    fn an_enterprise_installation_keeps_its_hostname() {
        let spec = zup_publish_github::RepositorySpec::parse("git.acme.internal/acme/acme")
            .expect("a three-segment spec");
        let resolved = resolve(
            Some(&spec),
            &Env(BTreeMap::new()),
            std::path::Path::new("."),
        )
        .expect("a repository");
        assert_eq!(resolved.repository.host.host, "git.acme.internal");
        assert_eq!(
            resolved.repository.host.api_base,
            "https://git.acme.internal/api/v3/"
        );
        assert_eq!(
            resolved.repository.host.upload_base,
            "https://git.acme.internal/uploads/"
        );
    }
}

/// Endpoints are modelled once, not spelled into format strings.
mod endpoints {
    use zup_publish_github::GithubHost;
    #[test]
    fn github_com_has_three_distinct_bases() {
        let host = GithubHost::dotcom();
        assert_eq!(host.web_base, "https://github.com/");
        assert_eq!(host.api_base, "https://api.github.com/");
        assert_eq!(host.upload_base, "https://uploads.github.com/");
        assert!(host.dotcom);
    }

    #[test]
    fn an_enterprise_installation_serves_all_three_from_one_host() {
        let host = GithubHost::enterprise("git.acme.internal").expect("an enterprise host");
        assert_eq!(host.web_base, "https://git.acme.internal/");
        assert_eq!(host.api_base, "https://git.acme.internal/api/v3/");
        assert_eq!(host.upload_base, "https://git.acme.internal/uploads/");
        assert!(!host.dotcom);
    }

    #[test]
    fn the_pinned_and_latest_addresses_are_different_shapes_of_url() {
        let host = GithubHost::dotcom();
        assert_eq!(
            host.download_url("acme/acme", "v1.4.0", "Acme-Windows-Setup.exe"),
            "https://github.com/acme/acme/releases/download/v1.4.0/Acme-Windows-Setup.exe"
        );
        assert_eq!(
            host.latest_download_url("acme/acme", "Acme-Windows-Setup.exe"),
            "https://github.com/acme/acme/releases/latest/download/Acme-Windows-Setup.exe"
        );
    }

    #[test]
    fn a_tag_with_a_space_is_encoded_rather_than_meaning_something_else() {
        let host = GithubHost::dotcom();
        assert_eq!(
            host.download_url("acme/acme", "v1.4.0 rc1", "a.exe"),
            "https://github.com/acme/acme/releases/download/v1.4.0%20rc1/a.exe"
        );
    }
}

/// Notes are GitHub's, and a body somebody wrote is not overwritten.
mod notes_policy {
    use zup_publish_github::NotesPolicy;

    #[test]
    fn the_default_is_githubs_own_generator() {
        assert_eq!(NotesPolicy::default().as_str(), "generated");
    }

    #[test]
    fn no_policy_writes_no_body() {
        assert!(
            zup_publish_github::notes_compose(
                &NotesPolicy::None,
                Some("x".into()),
                None,
                None,
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn a_generated_body_gains_a_download_table_and_keeps_its_text() {
        let rows = vec![zup_publish_github::DownloadRow {
            name: "Acme-Windows-Setup.exe".to_owned(),
            size: 418 * 1024 * 1024,
            role: "install".to_owned(),
        }];
        let body = zup_publish_github::notes_compose(
            &NotesPolicy::Generated,
            Some("## What's Changed\n* a fix\n".to_owned()),
            None,
            None,
            &rows,
        )
        .expect("a body");
        assert!(body.starts_with("## What's Changed"), "{body}");
        assert!(body.contains("Acme-Windows-Setup.exe"), "{body}");
        assert!(body.contains("418 MiB"), "{body}");
    }

    #[test]
    fn a_file_policy_sends_the_file_verbatim_plus_the_table() {
        let rows = vec![zup_publish_github::DownloadRow {
            name: "Acme.exe".to_owned(),
            size: 1,
            role: "install".to_owned(),
        }];
        let body = zup_publish_github::notes_compose(
            &NotesPolicy::File("CHANGELOG.md".to_owned()),
            None,
            Some("Curated notes.\n".to_owned()),
            None,
            &rows,
        )
        .expect("a body");
        assert!(body.starts_with("Curated notes."), "{body}");
    }
}

/// Credentials are discovered, and never printed.
mod credentials {
    use std::collections::BTreeMap;

    use zup_publish_github::Environment;
    use zup_publish_github::{Token, discover, discover_with};

    struct Env(BTreeMap<String, String>);

    impl Environment for Env {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    #[test]
    fn gh_token_wins_over_github_token() {
        let environment = Env(BTreeMap::from([
            ("GH_TOKEN".to_owned(), "from-gh".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "from-github".to_owned()),
        ]));
        let found = discover(&environment).expect("a token");
        assert_eq!(found.expose(), "from-gh");
        assert!(found.is_ambient());
    }

    #[test]
    fn an_empty_variable_is_not_a_credential() {
        let environment = Env(BTreeMap::from([("GH_TOKEN".to_owned(), "   ".to_owned())]));
        let error =
            discover_with(&environment, || None).expect_err("an empty value is not a token");
        assert!(error.to_string().contains("GH_TOKEN"), "{error}");
    }

    #[test]
    fn the_gh_fallback_is_only_reached_when_the_environment_has_nothing() {
        // The fallback is a process spawn, so it must never run when a token is
        // already in the environment.
        let environment = Env(BTreeMap::from([(
            "GITHUB_TOKEN".to_owned(),
            "t".to_owned(),
        )]));
        let found =
            discover_with(&environment, || panic!("the fallback must not run")).expect("a token");
        assert_eq!(found.expose(), "t");

        let empty = Env(BTreeMap::new());
        let found = discover_with(&empty, || Some("from-gh".to_owned())).expect("a token");
        assert_eq!(found.expose(), "from-gh");
        assert!(!found.is_ambient());
    }

    #[test]
    fn a_token_redacts_itself_in_debug() {
        let found = Token::new("ghp_supersecret");
        let rendered = format!("{found:?}");
        assert!(!rendered.contains("ghp_supersecret"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn the_authorization_header_is_a_bearer_token() {
        assert_eq!(
            zup_publish_github::authorization(&Token::new("abc")),
            "Bearer abc"
        );
    }
}

/// The generated workflow, structurally rather than as text.
mod workflow {
    use std::collections::BTreeMap;

    use zup_publish_github::MatrixTarget;
    use zup_publish_github::{WorkflowPolicy, generate};

    fn matrix() -> Vec<MatrixTarget> {
        vec![
            MatrixTarget::new("windows-x64", "x86_64-pc-windows-msvc"),
            MatrixTarget::new("windows-arm64", "aarch64-pc-windows-msvc"),
            MatrixTarget::new("linux-x64", "x86_64-unknown-linux-gnu"),
            MatrixTarget::new("linux-arm64", "aarch64-unknown-linux-gnu"),
            MatrixTarget::new("macos-arm64", "aarch64-apple-darwin"),
        ]
    }

    #[test]
    fn a_single_target_produces_a_single_matrix_entry() {
        let rendered = generate(
            &WorkflowPolicy::default(),
            &[MatrixTarget::new("windows-x64", "x86_64-pc-windows-msvc")],
        );
        assert_eq!(rendered.matches("- profile:").count(), 1);
        assert!(rendered.contains("runner: windows-latest"));
    }

    #[test]
    fn the_matrix_uses_native_runners_and_names_the_right_ones() {
        let rendered = generate(&WorkflowPolicy::default(), &matrix());
        assert!(rendered.contains("runner: windows-latest"), "{rendered}");
        assert!(rendered.contains("runner: windows-11-arm"), "{rendered}");
        assert!(rendered.contains("runner: ubuntu-latest"), "{rendered}");
        assert!(rendered.contains("runner: ubuntu-24.04-arm"), "{rendered}");
        assert!(rendered.contains("runner: macos-latest"), "{rendered}");
    }

    #[test]
    fn a_target_with_no_native_runner_cross_compiles_instead_of_lying() {
        let rendered = generate(
            &WorkflowPolicy::default(),
            &[MatrixTarget::new("windows-x86", "i686-pc-windows-msvc")],
        );
        assert!(rendered.contains("runner: windows-latest"), "{rendered}");
        assert!(
            !rendered.contains("runner: windows-11-arm"),
            "an i686 target is not an arm64 target: {rendered}"
        );
    }

    #[test]
    fn the_phases_are_explicit_and_ordered() {
        let rendered = generate(&WorkflowPolicy::default(), &matrix());
        let order: Vec<usize> = [
            "  plan:",
            "  build:",
            "  compose:",
            "  attest:",
            "  publish:",
        ]
        .iter()
        .map(|phase| {
            rendered
                .find(phase)
                .unwrap_or_else(|| panic!("{phase} missing"))
        })
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    }

    #[test]
    fn only_the_publish_job_can_write_to_the_repository() {
        let rendered = generate(&WorkflowPolicy::default(), &matrix());
        assert_eq!(rendered.matches("contents: write").count(), 1);
        assert!(rendered.contains("permissions:\n      contents: write"));
        assert!(rendered.contains("id-token: write"));
        assert!(rendered.contains("attestations: write"));
        assert!(!rendered.contains("write-all"));
    }

    #[test]
    fn attestations_can_be_turned_off_entirely() {
        let policy = WorkflowPolicy {
            attestations: false,
            ..WorkflowPolicy::default()
        };
        let rendered = generate(&policy, &matrix());
        assert!(!rendered.contains("  attest:"), "{rendered}");
        assert!(!rendered.contains("id-token: write"), "{rendered}");
        // The publish job must not depend on a job that no longer exists.
        assert!(rendered.contains("needs: [compose]"), "{rendered}");
    }

    #[test]
    fn signing_is_optional_and_lands_after_composition() {
        let without = generate(&WorkflowPolicy::default(), &matrix());
        assert!(!without.contains("- name: Sign"));
        let policy = WorkflowPolicy {
            signing: Some(zup_publish_github::Signing {
                command: "signtool sign /tr $URL $FILE".to_owned(),
            }),
            ..WorkflowPolicy::default()
        };
        let with = generate(&policy, &matrix());
        let compose = with.find("- name: Compose release").expect("compose");
        let sign = with.find("- name: Sign").expect("a sign step");
        let publish = with.find("Publish release").expect("a publish step");
        assert!(compose < sign, "signing is after composition");
        assert!(sign < publish, "signing is before publication");
        assert!(with.contains("signtool sign"));
    }

    #[test]
    fn an_environment_is_opt_in() {
        assert!(!generate(&WorkflowPolicy::default(), &matrix()).contains("environment:"));
        let policy = WorkflowPolicy {
            environment: Some("release".to_owned()),
            ..WorkflowPolicy::default()
        };
        assert!(generate(&policy, &matrix()).contains("environment: release"));
    }

    #[test]
    fn concurrency_never_cancels_a_half_published_release() {
        let rendered = generate(&WorkflowPolicy::default(), &matrix());
        assert!(rendered.contains("cancel-in-progress: false"), "{rendered}");
        assert!(rendered.contains("github.repository"), "{rendered}");
    }

    #[test]
    fn every_action_is_pinned_to_a_commit_sha() {
        let rendered = generate(&WorkflowPolicy::default(), &matrix());
        for line in rendered.lines().filter(|line| line.contains("uses: ")) {
            let reference = line.split("uses: ").nth(1).expect("a uses line").trim();
            let spec = reference.split('#').next().unwrap_or("").trim();
            let sha = spec.rsplit('@').next().expect("a reference");
            assert_eq!(sha.len(), 40, "`{line}` is not pinned to a full commit");
            assert!(
                sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "`{line}` is not a commit sha"
            );
            assert!(line.contains('#'), "`{line}` has no version comment");
        }
    }

    #[test]
    fn a_runner_override_wins_over_the_derived_mapping() {
        let policy = WorkflowPolicy {
            runner_overrides: BTreeMap::from([(
                "aarch64-pc-windows-msvc".to_owned(),
                "windows-11-vs2026-arm".to_owned(),
            )]),
            ..WorkflowPolicy::default()
        };
        let rendered = generate(
            &policy,
            &[MatrixTarget::new(
                "windows-arm64",
                "aarch64-pc-windows-msvc",
            )],
        );
        assert!(
            rendered.contains("runner: windows-11-vs2026-arm"),
            "{rendered}"
        );
    }

    #[test]
    fn generation_is_reproducible_and_check_notices_drift() {
        let policy = WorkflowPolicy::default();
        let first = generate(&policy, &matrix());
        let second = generate(&policy, &matrix());
        assert_eq!(first, second, "the same inputs produce the same bytes");

        let root = tempfile::tempdir().expect("a temporary directory");
        let freshness = zup_publish_github::check(root.path(), &policy, &matrix());
        assert!(!freshness.current);
        assert!(!freshness.present);
        assert!(freshness.detail.contains("does not exist"));

        let path = root.path().join(zup_publish_github::WORKFLOW_PATH);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, "name: something else\n").expect("the file is written");
        let stale = zup_publish_github::check(root.path(), &policy, &matrix());
        assert!(!stale.current);
        assert!(stale.detail.contains("no longer matches"));

        std::fs::write(&path, &first).expect("the file is rewritten");
        let current = zup_publish_github::check(root.path(), &policy, &matrix());
        assert!(current.current, "{}", current.detail);
    }
}
