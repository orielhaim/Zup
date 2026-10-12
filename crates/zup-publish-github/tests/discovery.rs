use rstest::rstest;
use std::collections::BTreeMap;
use zup_publish_github::{parse_remote_url, parse_remotes, resolve};

struct Env(BTreeMap<String, String>);

impl zup_publish_github::Environment for Env {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

#[rstest]
#[case("https://github.com/acme/acme.git")]
#[case("git@github.com:acme/acme.git")]
#[case("ssh://git@github.com:acme/acme.git")]
#[case("https://user@github.com/acme/acme")]
fn a_remote_url_is_read_as_a_host_and_two_names(#[case] url: &str) {
    assert_eq!(
        parse_remote_url(url),
        Some(("github.com".into(), "acme".into(), "acme".into())),
        "{url}"
    );
}

#[rstest]
#[case("https://gitlab.com/acme/acme.git")]
#[case("https://git.acme.internal/acme/acme.git")]
fn a_non_github_remote_is_not_a_candidate(#[case] url: &str) {
    assert!(parse_remote_url(url).is_none(), "{url} is not a candidate");
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
fn a_repository_is_chosen_by_precedence() {
    let working = |config: &str| {
        let root = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("a .git directory");
        std::fs::write(root.path().join(".git/config"), config).expect("the config");
        root
    };
    let environment = |repository: &str| {
        Env(BTreeMap::from([(
            "GITHUB_REPOSITORY".to_owned(),
            repository.to_owned(),
        )]))
    };
    let one_remote = "[remote \"origin\"]\n\turl = git@github.com:from-remote/acme.git\n";
    let spec = |spec: &str| zup_publish_github::RepositorySpec::parse(spec).expect("a spec");

    let git = working(one_remote);
    let explicit = spec("explicit/acme");
    assert_eq!(
        resolve(Some(&explicit), &environment("from-env/acme"), git.path())
            .expect("a repository")
            .repository
            .to_string(),
        "explicit/acme"
    );

    let git = working(one_remote);
    let resolved = resolve(None, &environment("from-env/acme"), git.path()).expect("a repository");
    assert_eq!(resolved.repository.to_string(), "from-env/acme");
    assert_eq!(resolved.discovery.as_str(), "github_repository");

    let git = working("[remote \"fork\"]\n\turl = git@github.com:acme/acme.git\n");
    let resolved = resolve(None, &Env(BTreeMap::new()), git.path()).expect("a repository");
    assert_eq!(resolved.repository.to_string(), "acme/acme");
    assert_eq!(resolved.remote.as_deref(), Some("fork"));

    let git = working(
        "[remote \"fork\"]\n\turl = git@github.com:acme/fork.git\n\
         [remote \"mirror\"]\n\turl = https://github.com/acme/acme.git\n",
    );
    let text = resolve(None, &Env(BTreeMap::new()), git.path())
        .expect_err("this is a question, not a guess")
        .to_string();
    assert!(text.contains("--repo"), "{text}");
    assert!(text.contains("fork"), "{text}");
}
