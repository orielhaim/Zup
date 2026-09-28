//! The parts that can only be checked against GitHub itself.
//!
//! Everything else in this crate runs against a mock, which is the right default:
//! a mock is deterministic, fast, and can be made to do the thing a real host
//! does. But a mock cannot tell you whether GitHub still serves
//! `releases/download/<tag>/<asset>` the way this crate assumes, whether the
//! documented `sha256:` digest field is still there, whether the upload endpoint
//! still wants the API-version media type, or whether a release asset answers a
//! `Range` request. Those are facts about a server that changes, and the only way
//! to know is to ask.
//!
//! # Running it
//!
//! Off by default, and off in a way that is a skip rather than a failure, so the
//! ordinary suite never touches the network:
//!
//! ```bash
//! ZUP_GITHUB_LIVE=1 \
//! ZUP_GITHUB_LIVE_REPO=owner/name \
//! GH_TOKEN=… \
//! cargo nextest run -p zup-publish-github --test live
//! ```
//!
//! `ZUP_GITHUB_LIVE_REPO` defaults to `GITHUB_REPOSITORY`, so a workflow that
//! already sets that needs only `ZUP_GITHUB_LIVE=1`.
//!
//! # What it will never do
//!
//! Nothing here creates, uploads, publishes, or deletes anything. The tests read
//! and they assert. A harness that could mutate a repository would eventually
//! mutate the wrong one, and the value of a live check does not justify a
//! maintainer having to wonder.

use zup_publish_github::{
    GithubClient, GithubRepository, ProcessEnvironment, RepositorySpec, discover, resolve,
};

/// The repository to ask about, or `None` to skip.
fn live_repository() -> Option<GithubRepository> {
    std::env::var_os("ZUP_GITHUB_LIVE")?;
    let spec = std::env::var("ZUP_GITHUB_LIVE_REPO")
        .or_else(|_| std::env::var("GITHUB_REPOSITORY"))
        .expect("ZUP_GITHUB_LIVE is set, so name a repository: ZUP_GITHUB_LIVE_REPO=owner/name");
    let environment = ProcessEnvironment;
    let resolved = resolve(
        Some(&RepositorySpec::parse(&spec).expect("a repository spec")),
        &environment,
        &std::env::temp_dir(),
    )
    .expect("the repository resolves");
    Some(resolved.repository)
}

/// A credential, or `None` to skip.
fn live_token() -> Option<zup_publish_github::Token> {
    discover(&ProcessEnvironment).ok()
}

macro_rules! or_skip {
    ($value:expr, $what:literal) => {
        match $value {
            Some(value) => value,
            None => {
                eprintln!("skipping: {}", $what);
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_real_repository_answers_the_calls_this_client_makes() {
    let repository = or_skip!(live_repository(), "no ZUP_GITHUB_LIVE repository");
    let token = or_skip!(live_token(), "no GitHub credential");
    let client = GithubClient::new(&repository, &token).expect("a client");

    // The two calls a read-only check makes. If either shape changed, this fails
    // here rather than in a maintainer's release.
    let info = client
        .repository_info()
        .await
        .expect("a real repository is readable");
    assert_eq!(
        info.full_name.to_lowercase(),
        format!("{}/{}", repository.owner, repository.name).to_lowercase(),
        "the API named the repository the client asked for"
    );
    assert!(
        !info.default_branch.is_empty(),
        "and reported a default branch, which is what a tag would be created from"
    );
    // `immutable_releases` is `None` on a server too old to have the field, and
    // "not reported" is not "not enabled" - so only the value, never the absence,
    // is asserted.
    if let Some(immutable) = info.immutable_releases {
        eprintln!(
            "{}/{} reports immutable releases: {immutable}",
            repository.owner, repository.name
        );
    }

    // `release_by_tag` on a tag that does not exist is a `404`, which is the
    // ordinary answer for a first release and the one the publisher branches on.
    let missing = client
        .release_by_tag("v0.0.0-zup-live-probe-does-not-exist")
        .await;
    assert!(
        missing.is_ok(),
        "an absent tag is a `None`, not an error: {missing:?}"
    );
    assert!(
        missing.expect("checked").is_none(),
        "and the release really is absent"
    );
}

#[tokio::test]
async fn a_real_release_asset_answers_a_range_request_or_says_it_does_not() {
    // The one fact this design cannot assume, and the reason the content source
    // probes rather than depends on it.
    let repository = or_skip!(live_repository(), "no ZUP_GITHUB_LIVE repository");
    let Some(url) = std::env::var("ZUP_GITHUB_LIVE_ASSET").ok().map(|value| {
        if value.starts_with("http") {
            value
        } else {
            repository
                .host
                .download_url(&repository.path(), "v1.4.0", &value)
        }
    }) else {
        eprintln!("skipping: set ZUP_GITHUB_LIVE_ASSET to the name of an asset to probe");
        return;
    };

    let client = zup_publish_http().expect("a client");
    let address = url::Url::parse(&url).expect("the asset address is a URL");
    let origin = zup_acquire_http::Origin::site(&address).expect("an origin");
    let response = client
        .send(
            &origin,
            zup_acquire_http::Request::get(&address)
                .range(zup_acquire_http::Range::between(0, 1023)),
            5,
        )
        .await
        .expect("the asset is fetchable");

    match response.status().as_u16() {
        206 => {
            let declared = response
                .header("content-range")
                .expect("a 206 that answers a range declares one");
            assert!(
                declared.starts_with("bytes 0-"),
                "GitHub served a range this build cannot check: {declared}"
            );
            eprintln!("this host serves byte ranges for release assets");
        }
        200 => {
            eprintln!(
                "this host does NOT serve byte ranges for release assets; \
                 the fallback path is what a project on it will use"
            );
        }
        other => panic!("a release asset answered {other}"),
    }
}

fn zup_publish_http() -> Result<zup_acquire_http::HttpClient, String> {
    zup_acquire_http::HttpClient::new(&zup_acquire_http::HttpClientConfig::default())
        .map_err(|error| error.to_string())
}
