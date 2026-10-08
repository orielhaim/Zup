use zup_publish_github::{
    GithubClient, GithubRepository, ProcessEnvironment, RepositorySpec, discover, resolve,
};

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
    if let Some(immutable) = info.immutable_releases {
        eprintln!(
            "{}/{} reports immutable releases: {immutable}",
            repository.owner, repository.name
        );
    }

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
