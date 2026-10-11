use std::collections::BTreeMap;

use zup_acquire_http::SecretHeader;
use zup_publish_github::Environment;
use zup_publish_github::{
    GithubError, GithubReceipt, GithubRepository, Token, authorization, discover_with,
};

struct Env(BTreeMap<String, String>);

impl Environment for Env {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

#[test]
fn a_credential_comes_from_the_environment_before_anything_else() {
    let environment = |pairs: &[(&str, &str)]| {
        Env(pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect())
    };

    let both = environment(&[("GH_TOKEN", "from-gh"), ("GITHUB_TOKEN", "from-github")]);
    let found = discover_with(&both, || panic!("the gh process must not run")).expect("a token");
    assert_eq!(found.expose(), "from-gh");
    assert!(found.is_ambient());

    let one = environment(&[("GITHUB_TOKEN", "from-github")]);
    let found = discover_with(&one, || panic!("the gh process must not run")).expect("a token");
    assert_eq!(found.expose(), "from-github");
    assert!(found.is_ambient());

    let found =
        discover_with(&Env(BTreeMap::new()), || Some("from-gh".to_owned())).expect("a token");
    assert_eq!(found.expose(), "from-gh");
    assert!(
        !found.is_ambient(),
        "a spawned credential is not the environment"
    );
}

#[test]
fn an_empty_variable_is_not_a_credential() {
    let environment = Env(BTreeMap::from([("GH_TOKEN".to_owned(), "   ".to_owned())]));
    let error = discover_with(&environment, || None).expect_err("an empty value is not a token");
    assert!(error.to_string().contains("GH_TOKEN"), "{error}");
}
#[test]
fn a_token_redacts_itself_in_debug() {
    let found = Token::new("ghp_supersecret");
    let rendered = format!("{found:?}");
    assert!(!rendered.contains("ghp_supersecret"), "{rendered}");
    assert!(rendered.contains("REDACTED"), "{rendered}");
}

#[test]
fn no_credential_reaches_a_debug_string_through_a_wrapper() {
    #[derive(Debug)]
    struct Request<'a> {
        repository: &'a str,
        token: &'a Token,
    }

    const SECRET: &str = "ghp_never_print_me";
    let token = Token::new(SECRET);
    let request = Request {
        repository: "acme/acme",
        token: &token,
    };
    assert_eq!(request.repository, "acme/acme");
    assert!(request.token.same_secret_as(&token));
    for rendered in [format!("{request:?}"), format!("{token:?}")] {
        assert!(!rendered.contains(SECRET), "{rendered}");
    }
}

#[test]
fn no_credential_reaches_a_report_a_receipt_or_a_diagnostic() {
    const SECRET: &str = "ghp_never_print_me";
    let repository = GithubRepository::dotcom("acme", "acme");

    let errors = [
        GithubError::NoToken,
        GithubError::Unauthenticated {
            reason: "the credential was rejected".to_owned(),
        },
        GithubError::Status {
            status: 401,
            message: Some("Bad credentials".to_owned()),
        },
        GithubError::RateLimited { seconds: Some(60) },
        GithubError::NotFound {
            what: "release asset".to_owned(),
        },
        GithubError::Digest {
            name: "Acme-Setup.exe".to_owned(),
            expected: "a".repeat(64),
            found: "b".repeat(64),
        },
        GithubError::PublishedConflict {
            tag: "v1.4.0".to_owned(),
        },
    ];
    for error in errors {
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SECRET), "{rendered}");
    }

    let receipt = GithubReceipt::new(&repository, "v1.4.0", 42);
    let encoded =
        String::from_utf8(receipt.encode().expect("a receipt encodes")).expect("a receipt is text");
    assert!(!encoded.contains(SECRET), "{encoded}");

    let diagnosis = zup_publish_github::Diagnosis {
        repository: repository.to_string(),
        host: repository.host.host.clone(),
        private: false,
        archived: false,
        immutable_releases: None,
        tag: "v1.4.0".to_owned(),
        assets: 3,
        asset_limit: 1000,
        largest: Some(("Acme-Setup.exe".to_owned(), 1)),
        asset_bytes_limit: 2 * 1024 * 1024 * 1024,
        release_exists: false,
        release_is_draft: false,
        release_is_immutable: None,
    };
    assert!(!format!("{diagnosis:?}").contains(SECRET));
}

#[test]
fn the_only_read_of_a_token_is_the_authorization_header() {
    const SECRET: &str = "ghp_never_print_me";
    let token = Token::new(SECRET);

    assert_eq!(authorization(&token), format!("Bearer {SECRET}"));

    let header = SecretHeader::new(authorization(&token));
    assert!(!format!("{header:?}").contains(SECRET));
}

#[test]
fn a_token_from_every_source_redacts_itself() {
    const SECRET: &str = "ghp_every_source";
    let sources = [
        Token::new(SECRET),
        discover_with(
            &Env(BTreeMap::from([("GH_TOKEN".to_owned(), SECRET.to_owned())])),
            || panic!("the fallback must not run"),
        )
        .expect("a token"),
        discover_with(
            &Env(BTreeMap::from([(
                "GITHUB_TOKEN".to_owned(),
                SECRET.to_owned(),
            )])),
            || panic!("the fallback must not run"),
        )
        .expect("a token"),
        discover_with(&Env(BTreeMap::new()), || Some(SECRET.to_owned())).expect("a token"),
    ];
    for token in &sources {
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(token.expose() == SECRET, "the value is still reachable");
    }
}

#[test]
fn comparing_two_tokens_is_explicit_about_comparing_secrets() {
    let left = Token::new("a");
    assert!(left.same_secret_as(&Token::new("a")));
    assert!(!left.same_secret_as(&Token::new("b")));
    assert!(!left.same_secret_as(&Token::new("aa")));
}
