use std::num::NonZeroU64;

use jiff::{SignedDuration, Timestamp};
use semver::Version;
use tough::TargetName;
use tough::editor::RepositoryEditor;
use tough::editor::signed::{PathExists, SignedRole};
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{KeyHolder, Root, Signed, Target};
use url::Url;
use zup_core::UpdateConfig;
use zup_update::{CheckResult, Client};

struct Fixture {
    root: tempfile::TempDir,
    config: UpdateConfig,
    target: String,
}

struct RepoOptions<'a> {
    corrupt_artifact: bool,
    app_id: &'a str,
    platform: &'a str,
    architecture: &'a str,
    release_version: &'a str,
    metadata_version: u64,
    expired_timestamp: bool,
    rotate_root: bool,
}

impl Default for RepoOptions<'_> {
    fn default() -> Self {
        Self {
            corrupt_artifact: false,
            app_id: "com.example.acme",
            platform: "windows",
            architecture: std::env::consts::ARCH,
            release_version: "1.1.0",
            metadata_version: 1,
            expired_timestamp: false,
            rotate_root: false,
        }
    }
}

async fn repository(options: RepoOptions<'_>) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let metadata = root.path().join("metadata");
    let targets = root.path().join("targets");
    let input = root.path().join("input");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::create_dir_all(&targets).unwrap();
    let trusted_root = include_bytes!("fixtures/root.json").to_vec();
    let root_file = root.path().join("root.json");
    std::fs::write(&root_file, &trusted_root).unwrap();
    let signing_key = root.path().join("signing.pem");
    std::fs::write(&signing_key, include_bytes!("fixtures/signing.pem")).unwrap();

    let descriptor = serde_json::json!({
        "schema": 1,
        "app_id": options.app_id,
        "channel": "stable",
        "version": options.release_version,
        "platform": options.platform,
        "architecture": options.architecture,
        "target": format!("artifacts/{}/windows-{}/Acme-Setup.exe", options.release_version, std::env::consts::ARCH),
    });
    let descriptor_path = input.join("channels/stable.json");
    let artifact_name = format!(
        "artifacts/{}/windows-{}/Acme-Setup.exe",
        options.release_version,
        std::env::consts::ARCH
    );
    let artifact_path = input.join(&artifact_name);
    std::fs::create_dir_all(descriptor_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
    std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
    std::fs::write(&artifact_path, b"verified setup executable fixture").unwrap();

    let mut editor = RepositoryEditor::new(&root_file).await.unwrap();
    editor
        .targets_version(NonZeroU64::new(options.metadata_version).unwrap())
        .unwrap()
        .targets_expires(Timestamp::now() + SignedDuration::from_hours(24 * 30))
        .unwrap()
        .snapshot_version(NonZeroU64::new(options.metadata_version).unwrap())
        .snapshot_expires(Timestamp::now() + SignedDuration::from_hours(24 * 30))
        .timestamp_version(NonZeroU64::new(options.metadata_version).unwrap());
    editor.timestamp_expires(if options.expired_timestamp {
        Timestamp::now() - SignedDuration::from_hours(1)
    } else {
        Timestamp::now() + SignedDuration::from_hours(24 * 7)
    });
    editor
        .add_target(
            "channels/stable.json",
            Target::from_path(&descriptor_path).await.unwrap(),
        )
        .unwrap();
    editor
        .add_target(
            artifact_name.as_str(),
            Target::from_path(&artifact_path).await.unwrap(),
        )
        .unwrap();
    let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource { path: signing_key })];
    let signed = editor.sign(&keys).await.unwrap();
    signed.write(&metadata).await.unwrap();
    if options.rotate_root {
        let mut root_v2: Signed<Root> = serde_json::from_slice(&trusted_root).unwrap();
        root_v2.signed.version = NonZeroU64::new(2).unwrap();
        let old_root: Signed<Root> = serde_json::from_slice(&trusted_root).unwrap();
        SignedRole::new(
            root_v2.signed,
            &KeyHolder::Root(old_root.signed),
            &keys,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .await
        .unwrap()
        .write(&metadata, true)
        .await
        .unwrap();
    }
    signed
        .link_target(
            &descriptor_path,
            &targets,
            PathExists::Replace,
            Some(&TargetName::new("channels/stable.json").unwrap()),
        )
        .await
        .unwrap();
    signed
        .link_target(
            &artifact_path,
            &targets,
            PathExists::Replace,
            Some(&TargetName::new(&artifact_name).unwrap()),
        )
        .await
        .unwrap();
    if options.corrupt_artifact {
        let target = find_named_target(&targets, "Acme-Setup.exe").unwrap();
        std::fs::write(target, b"corrupt").unwrap();
    }
    let mut repo_url = Url::from_directory_path(root.path()).unwrap();
    repo_url.set_query(None);
    Fixture {
        root,
        config: UpdateConfig {
            repository: repo_url.to_string(),
            channel: "stable".into(),
            trusted_root,
        },
        target: artifact_name,
    }
}

fn find_named_target(directory: &std::path::Path, suffix: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            if let Some(found) = find_named_target(&entry.path(), suffix) {
                return Some(found);
            }
        } else if entry.file_name().to_string_lossy().ends_with(suffix) {
            return Some(entry.path());
        }
    }
    None
}

#[tokio::test]
async fn checks_update_and_downloads_only_fully_verified_target() {
    let fixture = repository(RepoOptions::default()).await;
    let state = fixture.root.path().join("state");
    let client = Client::new(&fixture.config, "com.example.acme", &state);
    let result = client
        .check(&Version::parse("1.0.0").unwrap())
        .await
        .unwrap();
    assert_eq!(
        result,
        CheckResult::UpdateAvailable {
            current: Version::parse("1.0.0").unwrap(),
            available: Version::parse("1.1.0").unwrap(),
            target: fixture.target.clone(),
        }
    );
    let output = fixture.root.path().join("downloads/setup.exe");
    client.download(&fixture.target, &output).await.unwrap();
    assert_eq!(
        std::fs::read(output).unwrap(),
        b"verified setup executable fixture"
    );
    assert!(state.join("updates/com.example.acme/stable/tuf").is_dir());
}

#[tokio::test]
async fn corrupt_target_never_becomes_published_download() {
    let fixture = repository(RepoOptions {
        corrupt_artifact: true,
        ..RepoOptions::default()
    })
    .await;
    let state = fixture.root.path().join("state");
    let client = Client::new(&fixture.config, "com.example.acme", &state);
    let output = fixture.root.path().join("downloads/setup.exe");
    assert!(client.download(&fixture.target, &output).await.is_err());
    assert!(!output.exists());
    assert!(output.parent().unwrap().exists());
    assert!(
        std::fs::read_dir(output.parent().unwrap())
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn refuses_wrong_app_id_and_wrong_platform() {
    for (app_id, platform, architecture) in [
        ("com.example.other", "windows", std::env::consts::ARCH),
        ("com.example.acme", "linux", std::env::consts::ARCH),
        ("com.example.acme", "windows", "wrong-arch"),
    ] {
        let fixture = repository(RepoOptions {
            app_id,
            platform,
            architecture,
            ..RepoOptions::default()
        })
        .await;
        let state = fixture.root.path().join("state");
        let client = Client::new(&fixture.config, "com.example.acme", &state);
        assert!(
            client
                .check(&Version::parse("1.0.0").unwrap())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn up_to_date_and_downgrade_do_not_offer_installation() {
    let fixture = repository(RepoOptions::default()).await;
    let state = fixture.root.path().join("state");
    let client = Client::new(&fixture.config, "com.example.acme", &state);
    assert_eq!(
        client
            .check(&Version::parse("1.1.0").unwrap())
            .await
            .unwrap(),
        CheckResult::UpToDate {
            current: Version::parse("1.1.0").unwrap()
        }
    );
    assert_eq!(
        client
            .check(&Version::parse("1.2.0").unwrap())
            .await
            .unwrap(),
        CheckResult::UpToDate {
            current: Version::parse("1.2.0").unwrap()
        }
    );
}

#[tokio::test]
async fn rejects_expired_timestamp_and_metadata_rollback() {
    let expired = repository(RepoOptions {
        expired_timestamp: true,
        ..RepoOptions::default()
    })
    .await;
    let expired_state = expired.root.path().join("expired-state");
    let expired_client = Client::new(&expired.config, "com.example.acme", &expired_state);
    assert!(
        expired_client
            .check(&Version::parse("1.0.0").unwrap())
            .await
            .is_err()
    );

    let latest = repository(RepoOptions {
        release_version: "1.2.0",
        metadata_version: 2,
        ..RepoOptions::default()
    })
    .await;
    let older = repository(RepoOptions::default()).await;
    let state = latest.root.path().join("persistent-state");
    Client::new(&latest.config, "com.example.acme", &state)
        .check(&Version::parse("1.0.0").unwrap())
        .await
        .unwrap();
    assert!(
        Client::new(&older.config, "com.example.acme", &state)
            .check(&Version::parse("1.0.0").unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn corrupted_channel_descriptor_is_rejected_before_download() {
    let fixture = repository(RepoOptions::default()).await;
    let descriptor_target =
        find_named_target(&fixture.root.path().join("targets"), "stable.json").unwrap();
    std::fs::write(descriptor_target, b"{\"schema\":1}").unwrap();
    let state = fixture.root.path().join("state");
    let client = Client::new(&fixture.config, "com.example.acme", &state);
    assert!(
        client
            .check(&Version::parse("1.0.0").unwrap())
            .await
            .is_err()
    );
    let output = fixture.root.path().join("downloads/setup.exe");
    assert!(client.download(&fixture.target, &output).await.is_err());
    assert!(!output.exists());
}

#[tokio::test]
async fn rejects_snapshot_mixed_with_a_different_timestamp() {
    let fixture = repository(RepoOptions::default()).await;
    let metadata = fixture.root.path().join("metadata");
    let snapshot = std::fs::read_dir(&metadata)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("snapshot.json")
        })
        .unwrap()
        .path();
    std::fs::write(snapshot, b"{}").unwrap();
    let state = fixture.root.path().join("state");
    let client = Client::new(&fixture.config, "com.example.acme", &state);
    assert!(
        client
            .check(&Version::parse("1.0.0").unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn follows_a_sequential_root_key_rotation() {
    let fixture = repository(RepoOptions {
        rotate_root: true,
        ..RepoOptions::default()
    })
    .await;
    let state = fixture.root.path().join("state");
    let result = Client::new(&fixture.config, "com.example.acme", &state)
        .check(&Version::parse("1.0.0").unwrap())
        .await;
    assert!(result.is_ok(), "{result:?}");
}
