use rstest::rstest;
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

#[rstest]
#[case("x86_64-pc-windows-msvc", "windows-latest")]
#[case("aarch64-pc-windows-msvc", "windows-11-arm")]
#[case("x86_64-unknown-linux-gnu", "ubuntu-latest")]
#[case("aarch64-unknown-linux-gnu", "ubuntu-24.04-arm")]
#[case("aarch64-apple-darwin", "macos-latest")]
#[case("i686-pc-windows-msvc", "windows-latest")]
fn a_runner_is_derived_from_the_target(#[case] triple: &str, #[case] runner: &str) {
    let rendered = generate(
        &WorkflowPolicy::default(),
        &[MatrixTarget::new("the-target", triple)],
    );
    assert!(
        rendered.contains(&format!("runner: {runner}")),
        "{triple} is not built on {runner}:\n{rendered}"
    );
    assert_eq!(
        rendered.matches("- profile:").count(),
        1,
        "one target is one matrix entry:\n{rendered}"
    );
}

#[test]
fn a_runner_override_wins_over_the_default() {
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
fn the_phases_are_explicit_and_ordered() {
    let unsigned = generate(&WorkflowPolicy::default(), &matrix());
    for phase in [
        "  plan:",
        "  build:",
        "  compose:",
        "  attest:",
        "  publish:",
    ] {
        assert!(unsigned.contains(phase), "{phase} missing from\n{unsigned}");
    }
    assert!(
        !unsigned.contains("  sign:"),
        "nothing to sign with means no signing job:\n{unsigned}"
    );
    assert!(
        unsigned
            .find("operation: finalize")
            .expect("a finalize step")
            < unsigned.find("operation: attest").expect("attest"),
        "finalization precedes attestation"
    );

    let signed = generate(
        &WorkflowPolicy {
            signing: Some(zup_publish_github::Signing {
                command: "signtool sign $FILE".to_owned(),
            }),
            ..WorkflowPolicy::default()
        },
        &matrix(),
    );
    let order: Vec<usize> = [
        "  plan:",
        "  build:",
        "  compose:",
        "  sign:",
        "  attest:",
        "  publish:",
    ]
    .iter()
    .map(|phase| {
        signed
            .find(phase)
            .unwrap_or_else(|| panic!("{phase} missing from\n{signed}"))
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
        signing: Some(zup_publish_github::Signing {
            command: "signtool sign $FILE".to_owned(),
        }),
        ..WorkflowPolicy::default()
    };
    let rendered = generate(&policy, &matrix());
    assert!(!rendered.contains("  attest:"), "{rendered}");
    assert!(!rendered.contains("id-token: write"), "{rendered}");
    assert!(rendered.contains("needs: [compose, sign]"), "{rendered}");
}

#[test]
fn nothing_downstream_of_composition_runs_before_the_release_is_finalized() {
    let line_of = |rendered: &str, needle: &str| {
        rendered
            .lines()
            .position(|line| line.trim() == needle)
            .unwrap_or_else(|| panic!("no line `{needle}` in\n{rendered}"))
    };

    let signed = generate(
        &WorkflowPolicy {
            signing: Some(zup_publish_github::Signing {
                command: "signtool sign /tr $URL $FILE".to_owned(),
            }),
            ..WorkflowPolicy::default()
        },
        &matrix(),
    );
    let sign = line_of(&signed, "sign:");
    let unsigned_handover = line_of(&signed, "- name: Upload the release");
    assert!(
        unsigned_handover < sign,
        "composition hands the tree over before the signing job consumes it"
    );
    assert!(
        line_of(&signed, "- name: Upload the signed release") > sign,
        "the finalized tree is uploaded by the job that verified it"
    );
    for job in ["attest:", "publish:"] {
        let start = line_of(&signed, job);
        let collects = signed
            .lines()
            .skip(start)
            .any(|line| line.trim() == "name: compose" && line.starts_with("          "));
        assert!(collects, "{job} does not collect the finalized release");
    }

    let unsigned = generate(&WorkflowPolicy::default(), &matrix());
    let finalize = unsigned
        .lines()
        .position(|line| line.trim() == "operation: finalize")
        .expect("an unsigned pipeline still finalizes");
    let upload = line_of(&unsigned, "- name: Upload the release");
    assert!(
        finalize < upload,
        "the release is uploaded before it is finalized:\n{unsigned}"
    );
}

#[test]
fn signing_happens_in_its_own_job_between_composition_and_publication() {
    let policy = WorkflowPolicy {
        signing: Some(zup_publish_github::Signing {
            command: "signtool sign /fd SHA256 /tr $URL /td SHA256 $FILE".to_owned(),
        }),
        ..WorkflowPolicy::default()
    };
    let with = generate(&policy, &matrix());
    let compose = with.find("operation: compose").expect("compose");
    let sign = with.find("- name: Sign").expect("a sign step");
    let finalize = with.find("operation: finalize").expect("a finalize step");
    let publish = with.find("operation: publish").expect("a publish step");
    assert!(compose < sign, "signing is after composition");
    assert!(
        sign < finalize,
        "verification reads the bytes signing produced, so it comes after"
    );
    assert!(
        finalize < publish,
        "publication reads the finalized release"
    );
    assert!(with.contains("signtool sign"));
    assert!(with.contains("name: compose-unsigned"), "{with}");
    assert!(with.contains("runs-on: windows-latest"), "{with}");
}

#[test]
fn concurrency_never_cancels_a_half_published_release() {
    let rendered = generate(&WorkflowPolicy::default(), &matrix());
    assert!(rendered.contains("cancel-in-progress: false"), "{rendered}");
    assert!(rendered.contains("github.repository"), "{rendered}");
}

#[test]
fn every_third_party_action_uses_the_ref_the_lock_tracks() {
    let rendered = generate(&WorkflowPolicy::default(), &matrix());
    let action = WorkflowPolicy::default().action;
    let mut checked = 0;
    for line in rendered.lines().filter(|line| line.contains("uses: ")) {
        let reference = line.split("uses: ").nth(1).expect("a uses line").trim();
        if reference.starts_with(&action) {
            assert_eq!(reference, action, "`{line}` is not the configured action");
            continue;
        }
        let (repository, revision) = reference
            .split_once('@')
            .unwrap_or_else(|| panic!("`{line}` has no ref"));
        let locked = zup_publish_github::pin(repository.trim())
            .unwrap_or_else(|error| panic!("`{line}`: {error}"));
        assert_eq!(
            revision, locked.version,
            "`{line}` does not use the ref the lock tracks"
        );
        assert_eq!(locked.sha.len(), 40, "`{line}` has no recorded commit");
        checked += 1;
    }
    assert!(
        checked >= 4,
        "only {checked} tracked references were checked"
    );
}

#[test]
fn no_generated_reference_is_a_bare_commit_sha() {
    let rendered = generate(&WorkflowPolicy::default(), &matrix());
    for line in rendered.lines().filter(|line| line.contains("uses: ")) {
        let reference = line.split("uses: ").nth(1).expect("a uses line").trim();
        let revision = reference
            .split_once('@')
            .map(|(_, revision)| revision)
            .unwrap_or(reference);
        assert!(
            !(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit())),
            "`{line}` pins a commit rather than a version ref"
        );
    }
}

#[test]
fn the_token_is_an_action_input_rather_than_a_step_environment() {
    let rendered = generate(&WorkflowPolicy::default(), &matrix());
    assert!(
        !rendered.contains("GITHUB_TOKEN: ${{ secrets"),
        "the token is exposed as an environment variable:\n{rendered}"
    );
    assert!(
        rendered.contains("github-token: ${{ secrets.GITHUB_TOKEN }}"),
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
