#[path = "../../zup-distribute-github/tests/common/mod.rs"]
mod distribution;
#[path = "../../zup-publish-github/tests/common/mod.rs"]
mod github;
#[cfg(windows)]
#[path = "../../zup-windows/tests/fixture/junction.rs"]
mod junction;

use crate::publish::{self, Staged};
use clap::Parser as _;
use zup_artifact::{BuildArtifact, ReleaseManifest, ReleaseTransport};
use zup_core::Target;

const PROJECT: &str = r#"
schema = 1
[app]
id = "com.acme.app"
name = "Acme"
version = "1.4.0"
[build.targets.windows-x64]
target = "windows-x64"
source = { directory = "payload" }
[build.targets.windows-arm64]
target = "windows-arm64"
source = { directory = "payload" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/Acme"
[[files]]
id = "application"
source = "**/*"
placement = { kind = "fixed", directory = "${install}" }
"#;

struct Fixture {
    root: tempfile::TempDir,
    project: zup_manifest::Manifest,
    release: ReleaseManifest,
    staged: Staged,
    blobs: Vec<distribution::Blob>,
}

impl Fixture {
    fn new(shard_bytes: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("zup.toml"), PROJECT).unwrap();
        let project = zup_manifest::parse(PROJECT).unwrap();
        let mut release = ReleaseManifest::new(&project.app);
        release.source = Some(zup_artifact::BuildSource {
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            repository: "github.com/acme/acme".into(),
        });
        let staged = Staged {
            release_dir: root.path().join("release"),
            packages: Some(root.path().join("packages")),
            web: None,
        };
        std::fs::create_dir_all(&staged.release_dir).unwrap();
        let tree = root.path().join("tree");
        let blobs = distribution::blobs(3, 32768);
        for blob in &blobs {
            let path = tree.join(zup_acquire::blob_path(&blob.digest).to_string());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &blob.compressed).unwrap();
        }
        let refs = blobs
            .iter()
            .map(|b| (b.digest, b.compressed.len() as u64, b.bytes.len() as u64))
            .collect::<Vec<_>>();
        let doc = zup_distribute_github::Document {
            digest: zup_core::hash_bytes(b"document"),
            size: 8,
        };
        for target in [Target::WINDOWS_X64, Target::WINDOWS_ARM64] {
            let id = target.as_str();
            let name = format!("{id}-setup.exe");
            let bytes = format!("finalized installer for {id}");
            let path = staged.release_dir.join(&name);
            std::fs::write(&path, &bytes).unwrap();
            release
                .add_single_target(
                    &zup_artifact::SingleTarget {
                        id: id.into(),
                        version: project.app.version.clone(),
                        target,
                        platform: zup_artifact::Platform::from_target(&target),
                        frontend: zup_core::Frontend::Gui,
                        subsystem: zup_artifact::LauncherSubsystem::Gui,
                        file_count: 3,
                        prerequisite_count: 0,
                        plugin_count: 0,
                    },
                    &name,
                    zup_artifact::Measured::single(
                        zup_core::hash_bytes(bytes.as_bytes()),
                        bytes.len() as u64,
                        bytes.len() as u64,
                    ),
                )
                .unwrap();
            release
                .finalize(
                    &staged.release_dir,
                    id,
                    &zup_signing::Measured::of(&path).unwrap(),
                    Vec::new(),
                )
                .unwrap();
            let written = crate::packages::write(
                &crate::packages::Request {
                    variant: id,
                    target: id,
                    application: project.app.id.as_str(),
                    manifest: doc,
                    catalog: doc,
                    blobs: &refs,
                },
                &tree,
                staged.packages.as_ref().unwrap(),
                "Acme",
                shard_bytes,
            )
            .unwrap();
            let descriptor = staged.packages.as_ref().unwrap().join(&written.descriptor);
            let (size, digest) =
                zup_core::hash_reader(std::fs::File::open(descriptor).unwrap()).unwrap();
            release.transport.push(ReleaseTransport {
                variant: id.into(),
                descriptor: written.descriptor,
                identity: BuildArtifact { digest, size },
            });
        }
        let fixture = Self {
            root,
            project,
            release,
            staged,
            blobs,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        std::fs::write(
            self.staged
                .release_dir
                .join(zup_artifact::RELEASE_MANIFEST_NAME),
            self.release.encode().unwrap(),
        )
        .unwrap();
    }

    fn plan(&self) -> miette::Result<publish::PublicationPlan> {
        publish::build_plan(
            &self.project,
            &zup_publish_github::PublishConfig::empty(),
            &self.release,
            &self.staged,
            &zup_publish::TagIntent::required("v1.4.0"),
            &zup_publish_github::LIMITS,
        )
    }
}

#[test]
fn staged_multivariant_transport_reaches_upload_receipt_and_verified_consumer() {
    let fixture = Fixture::new(20000);
    let planned = fixture.plan().unwrap();
    let products = planned.plan.products().unwrap();
    assert_eq!(products.len(), 11); // two installers, one manifest, two descriptors, six shards
    for product in &products {
        let path = &planned.sources[&product.name];
        assert_eq!(
            zup_core::hash_reader(std::fs::File::open(path).unwrap()).unwrap(),
            (product.size, product.digest)
        );
    }
    let github = github::Github::start(github::Behaviour::absent());
    let receipt_path = fixture.root.path().join("receipt.json");
    let mut options = publish::Options {
        receipt: Some(receipt_path.clone()),
        endpoints: Some(zup_publish_github::ClientEndpoints {
            api: github.api_origin(),
            upload: github.upload_origin(),
        }),
        ..Default::default()
    };
    options.dry_run = true;
    publish::run(
        &fixture.project,
        &zup_publish_github::PublishConfig::empty(),
        &fixture.staged,
        &github.repository(),
        &zup_publish_github::supplied("fixture-token"),
        &options,
    )
    .unwrap();
    assert!(github.log().iter().all(|s| s.starts_with("GET ")));
    options.dry_run = false;
    publish::run(
        &fixture.project,
        &zup_publish_github::PublishConfig::empty(),
        &fixture.staged,
        &github.repository(),
        &zup_publish_github::supplied("fixture-token"),
        &options,
    )
    .unwrap();
    let receipt =
        zup_publish_github::GithubReceipt::parse(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt.assets.len(), products.len());
    for asset in &receipt.assets {
        let product = products.iter().find(|p| p.name == asset.name).unwrap();
        assert_eq!((asset.size, asset.digest), (product.size, product.digest));
    }
    let uploads = github.upload_attempts();
    publish::run(
        &fixture.project,
        &zup_publish_github::PublishConfig::empty(),
        &fixture.staged,
        &github.repository(),
        &zup_publish_github::supplied("fixture-token"),
        &options,
    )
    .unwrap();
    assert_eq!(github.upload_attempts(), uploads);
    let repeated =
        zup_publish_github::GithubReceipt::parse(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt.source, repeated.source);
    let origin = distribution::Origin::start(github.public_assets());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for transport in &fixture.release.transport {
            let descriptor = zup_distribute_github::PackageDescriptor::parse(
                &github.public_assets()[&transport.descriptor],
                Some(transport.identity.digest),
            )
            .unwrap();
            assert!(descriptor.is_sharded());
            let layout = origin.pinned();
            let client = zup_distribute_github::client().unwrap();
            let package = zup_distribute_github::open(&layout, &descriptor, &client)
                .await
                .unwrap();
            let catalog = zup_acquire::ContentCatalog::new(
                fixture
                    .blobs
                    .iter()
                    .map(distribution::catalog_entry)
                    .collect(),
            )
            .unwrap();
            let source = zup_distribute_github::GithubContentSource::new(
                "release", &layout, package, catalog,
            )
            .unwrap();
            let chain = zup_acquire::SourceChain::new(vec![std::sync::Arc::new(source)]);
            let cache_root = tempfile::tempdir().unwrap();
            let cache =
                zup_acquire::ContentCache::open(cache_root.path(), zup_acquire::CachePolicy::Keep)
                    .unwrap();
            for blob in &fixture.blobs {
                let verified = chain
                    .acquire(
                        &distribution::descriptor(blob),
                        &cache,
                        &zup_acquire::NeverCancelled,
                    )
                    .await
                    .unwrap();
                assert_eq!(verified.read_to_end().unwrap(), blob.bytes);
            }
        }
    });
}

#[rstest::rstest]
#[case("corrupt")]
#[case("missing")]
#[case("truncated")]
#[case("shuffled")]
#[case("extra")]
#[case("foreign")]
#[case("descriptor")]
#[case("forged-checksums")]
fn transport_damage_is_refused_before_publication(#[case] damage: &str) {
    let mut fixture = Fixture::new(20000);
    let entry = &fixture.release.transport[0];
    let descriptor_path = fixture
        .staged
        .packages
        .as_ref()
        .unwrap()
        .join(&entry.descriptor);
    let mut descriptor = zup_distribute_github::PackageDescriptor::parse(
        &std::fs::read(&descriptor_path).unwrap(),
        None,
    )
    .unwrap();
    let first = fixture
        .staged
        .packages
        .as_ref()
        .unwrap()
        .join(&descriptor.shards[0].name);
    match damage {
        "corrupt" => std::fs::write(first, b"forged checksum content").unwrap(),
        "missing" => std::fs::remove_file(first).unwrap(),
        "truncated" => {
            let bytes = std::fs::read(&first).unwrap();
            std::fs::write(first, &bytes[..bytes.len() - 1]).unwrap();
        }
        "extra" => std::fs::write(
            fixture
                .staged
                .packages
                .as_ref()
                .unwrap()
                .join("foreign.zup.999"),
            b"foreign",
        )
        .unwrap(),
        "shuffled" | "foreign" => {
            if damage == "shuffled" {
                descriptor.shards.swap(0, 1);
            } else {
                descriptor.application = "foreign.app".into();
            }
            let bytes = descriptor.encode().unwrap();
            std::fs::write(descriptor_path, &bytes).unwrap();
            fixture.release.transport[0].identity = BuildArtifact {
                digest: zup_core::hash_bytes(&bytes),
                size: bytes.len() as u64,
            };
        }
        "descriptor" => std::fs::write(descriptor_path, b"{}").unwrap(),
        "forged-checksums" => {
            let mut bytes = std::fs::read(&first).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            std::fs::write(&first, &bytes).unwrap();
            descriptor.shards[0].digest = zup_core::hash_bytes(&bytes);
            let logical = descriptor
                .shards
                .iter()
                .flat_map(|s| {
                    std::fs::read(fixture.staged.packages.as_ref().unwrap().join(&s.name)).unwrap()
                })
                .collect::<Vec<_>>();
            descriptor.package.digest = zup_core::hash_bytes(&logical);
            let bytes = descriptor.encode().unwrap();
            std::fs::write(descriptor_path, &bytes).unwrap();
            fixture.release.transport[0].identity = BuildArtifact {
                digest: zup_core::hash_bytes(&bytes),
                size: bytes.len() as u64,
            };
        }
        _ => unreachable!(),
    }
    fixture.save();
    assert!(fixture.plan().is_err());
}

#[test]
fn cli_generation_and_freshness_use_the_fixture_manifest() {
    let fixture = Fixture::new(u64::MAX);
    let manifest = fixture.root.path().join("zup.toml");
    for verb in ["generate", "check"] {
        let cli = crate::cli::Cli::try_parse_from([
            "zup",
            "ci",
            "github",
            verb,
            "--manifest",
            manifest.to_str().unwrap(),
        ])
        .unwrap();
        crate::cli::dispatch(cli).unwrap();
    }
    let workflow = fixture.root.path().join(zup_publish_github::WORKFLOW_PATH);
    std::fs::write(workflow, "jobs: {}\n").unwrap();
    let cli = crate::cli::Cli::try_parse_from([
        "zup",
        "ci",
        "github",
        "check",
        "--manifest",
        manifest.to_str().unwrap(),
    ])
    .unwrap();
    assert!(crate::cli::dispatch(cli).is_err());
    assert_eq!(fixture.plan().unwrap().plan.products().unwrap().len(), 7);
}

#[rstest::rstest]
#[case("path")]
#[case("variant")]
#[case("target")]
#[case("source")]
#[case("application")]
#[case("missing-descriptor")]
fn inconsistent_manifest_claims_cannot_reach_publication(#[case] damage: &str) {
    let mut fixture = Fixture::new(20000);
    match damage {
        "path" => fixture.release.artifacts[0].path = "../foreign.exe".into(),
        "variant" => fixture.release.artifacts[0].variants.push("foreign".into()),
        "target" => {
            fixture.release.variants[0].target = Target::LINUX_X64;
            fixture.release.variants[0].platform =
                zup_artifact::Platform::from_target(&Target::LINUX_X64);
        }
        "source" => fixture.release.source.as_mut().unwrap().commit = "main".into(),
        "application" => {
            fixture.release.application.id = zup_core::AppId::new("foreign.app").unwrap()
        }
        "missing-descriptor" => {
            fixture.release.transport.pop();
        }
        _ => unreachable!(),
    }
    fixture.save();
    assert!(fixture.plan().is_err());
}

#[test]
fn requested_signing_cannot_publish_an_unsigned_finalization() {
    let fixture = Fixture::new(20000);
    let mut config = zup_publish_github::PublishConfig::empty();
    config.workflow.signing = Some(zup_publish_github::Signing {
        command: "sign-release".into(),
    });
    assert!(
        publish::build_plan(
            &fixture.project,
            &config,
            &fixture.release,
            &fixture.staged,
            &zup_publish::TagIntent::required("v1.4.0"),
            &zup_publish_github::LIMITS
        )
        .is_err()
    );
}

#[test]
fn changed_source_repository_is_refused_before_contacting_the_host() {
    let mut fixture = Fixture::new(20000);
    fixture.release.source.as_mut().unwrap().repository = "github.com/foreign/app".into();
    fixture.save();
    let github = github::Github::start(github::Behaviour::absent());
    let options = publish::Options {
        endpoints: Some(zup_publish_github::ClientEndpoints {
            api: github.api_origin(),
            upload: github.upload_origin(),
        }),
        ..Default::default()
    };
    assert!(
        publish::run(
            &fixture.project,
            &zup_publish_github::PublishConfig::empty(),
            &fixture.staged,
            &github.repository(),
            &zup_publish_github::supplied("fixture-token"),
            &options
        )
        .is_err()
    );
    assert_eq!(github.requests(), 0);
}

#[rstest::rstest]
#[case(0)]
#[case(1)]
#[case(1024)]
fn impossible_shard_limits_emit_no_partial_package(#[case] limit: u64) {
    let mut writer = zup_distribute_github::Writer::new();
    let blob = distribution::blobs(1, 32768).remove(0);
    writer
        .insert(blob.digest, blob.compressed, blob.bytes.len() as u64)
        .unwrap();
    let doc = zup_distribute_github::Document {
        digest: zup_core::hash_bytes(b"doc"),
        size: 3,
    };
    let mut emitted = 0;
    let result = writer.write(
        zup_distribute_github::PackageHeader::new(
            "windows-x64",
            "windows-x64",
            "com.acme.app",
            doc,
            doc,
        ),
        limit,
        |_, _| {
            emitted += 1;
            Ok(())
        },
    );
    assert!(matches!(
        result,
        Err(zup_distribute_github::PackageError::ShardLimit { .. })
    ));
    assert_eq!(emitted, 0);
}

#[test]
fn linked_artifact_directories_are_not_adopted() {
    let mut fixture = Fixture::new(20000);
    let outside = tempfile::tempdir().unwrap();
    let artifact = &mut fixture.release.artifacts[0];
    let bytes = std::fs::read(fixture.staged.release_dir.join(&artifact.path)).unwrap();
    std::fs::write(outside.path().join("setup.exe"), bytes).unwrap();
    let link = fixture.staged.release_dir.join("linked");
    #[cfg(windows)]
    let _junction = junction::Junction::new(&link, outside.path());
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    artifact.path = "linked/setup.exe".into();
    fixture.save();
    assert!(fixture.plan().is_err());
}

#[test]
fn manifest_unknown_fields_and_duplicate_source_claims_are_refused() {
    let fixture = Fixture::new(20000);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fixture.release.encode().unwrap()).unwrap();
    value["adopt-files-recursively"] = serde_json::json!(true);
    assert!(ReleaseManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let text = String::from_utf8(fixture.release.encode().unwrap()).unwrap();
    let duplicate = text.replacen('{', "{\"source\":null,", 1);
    assert!(ReleaseManifest::parse(duplicate.as_bytes()).is_err());
}

#[test]
fn build_source_records_clean_checkout_identity_across_branch_switch_and_shallow_clone() {
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let blank = root.path().join("empty-config");
    std::fs::write(&blank, "").unwrap();
    let git = |directory: &std::path::Path, args: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &blank)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=disabled-hooks",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&repository, &["init"]);
    let manifest = repository.join("zup.toml");
    std::fs::write(&manifest, PROJECT).unwrap();
    git(&repository, &["add", "zup.toml"]);
    git(&repository, &["commit", "-m", "fixture A"]);
    git(
        &repository,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/acme.git",
        ],
    );
    let source_a = publish::build_source(&manifest).unwrap().unwrap();
    git(&repository, &["switch", "-c", "other"]);
    std::fs::write(&manifest, format!("{PROJECT}\n# changed source\n")).unwrap();
    assert!(publish::build_source(&manifest).unwrap().is_none());
    git(&repository, &["add", "zup.toml"]);
    git(&repository, &["commit", "-m", "fixture B"]);
    let source_b = publish::build_source(&manifest).unwrap().unwrap();
    assert_ne!(source_a.commit, source_b.commit);
    let shallow = root.path().join("shallow");
    git(
        root.path(),
        &[
            "clone",
            "--depth=1",
            "--no-local",
            repository.to_str().unwrap(),
            shallow.to_str().unwrap(),
        ],
    );
    assert_eq!(
        git(&shallow, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    git(
        &shallow,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/acme.git",
        ],
    );
    assert_eq!(
        publish::build_source(&shallow.join("zup.toml")).unwrap(),
        Some(source_b)
    );
}

#[test]
fn actual_cli_build_compose_finalize_and_fake_publication_use_only_matrix_bytes() {
    let root = tempfile::tempdir().unwrap();
    let target = Target::host().unwrap();
    let manifest = root.path().join("zup.toml");
    let text = format!(
        r#"
schema = 1
[app]
id = "com.acme.app"
name = "Acme"
version = "1.4.0"
[build.targets.native]
target = "{target}"
frontend = "console"
source = {{ directory = "payload" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/Acme"
[[files]]
id = "application"
source = "**/*"
placement = {{ kind = "fixed", directory = "${{install}}" }}
executable = true
"#
    );
    std::fs::write(&manifest, text).unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir(&payload).unwrap();
    for (index, blob) in distribution::blobs(3, 32768).iter().enumerate() {
        std::fs::write(payload.join(format!("payload-{index}")), &blob.bytes).unwrap();
    }
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=disabled-hooks",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    git(&["add", "zup.toml", "payload"]);
    git(&["commit", "-m", "fixture source"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/acme.git",
    ]);
    let source = publish::build_source(&manifest).unwrap().unwrap();
    let release_dir = root.path().join("release");
    let variant = release_dir.join("variants/native");
    let executable = std::env::current_exe().unwrap();
    let toolchain = executable
        .ancestors()
        .map(|p| p.join("toolchain/0.0.1"))
        .find(|p| p.is_dir())
        .expect("stage native toolchain with cargo xtask toolchain build");
    let invoke = |args: Vec<String>| {
        crate::cli::dispatch(crate::cli::Cli::try_parse_from(args).unwrap()).unwrap();
    };
    for operation in ["generate", "check"] {
        invoke(vec![
            "zup".into(),
            "ci".into(),
            "github".into(),
            operation.into(),
            "--manifest".into(),
            manifest.display().to_string(),
        ]);
    }
    invoke(vec![
        "zup".into(),
        "--toolchain".into(),
        toolchain.display().to_string(),
        "build".into(),
        "--manifest".into(),
        manifest.display().to_string(),
        "--output-dir".into(),
        variant.display().to_string(),
    ]);
    std::fs::remove_dir_all(&payload).unwrap();
    let web = release_dir.join("web");
    let packages = release_dir.join("packages");
    invoke(vec![
        "zup".into(),
        "publish".into(),
        "stage".into(),
        "--manifest".into(),
        manifest.display().to_string(),
        "--release-dir".into(),
        release_dir.display().to_string(),
        "--output".into(),
        web.display().to_string(),
        "--packages".into(),
        packages.display().to_string(),
        "--shard-bytes".into(),
        "20000".into(),
    ]);
    invoke(vec![
        "zup".into(),
        "sign".into(),
        "--release-dir".into(),
        release_dir.display().to_string(),
        "verify".into(),
        "--allow-unsigned".into(),
    ]);
    let release = ReleaseManifest::parse(
        &std::fs::read(release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(release.source, Some(source.clone()));
    assert!(release.is_finalized());
    assert!(!release.objects.is_empty());
    let staged = Staged {
        release_dir,
        web: Some(web),
        packages: Some(packages),
    };
    let github = github::Github::start(github::Behaviour {
        tag_commit: Some(source.commit),
        ..github::Behaviour::absent()
    });
    let config = zup_publish_github::PublishConfig::empty();
    let mut options = publish::Options {
        dry_run: true,
        receipt: Some(root.path().join("receipt.json")),
        endpoints: Some(zup_publish_github::ClientEndpoints {
            api: github.api_origin(),
            upload: github.upload_origin(),
        }),
        ..Default::default()
    };
    publish::run(
        &zup_manifest::parse_named(&std::fs::read_to_string(&manifest).unwrap(), "zup.toml")
            .unwrap(),
        &config,
        &staged,
        &github.repository(),
        &zup_publish_github::supplied("fixture-token"),
        &options,
    )
    .unwrap();
    assert_eq!(github.upload_attempts(), 0);
    options.dry_run = false;
    for _ in 0..2 {
        publish::run(
            &zup_manifest::parse_named(&std::fs::read_to_string(&manifest).unwrap(), "zup.toml")
                .unwrap(),
            &config,
            &staged,
            &github.repository(),
            &zup_publish_github::supplied("fixture-token"),
            &options,
        )
        .unwrap();
    }
    let origin = distribution::Origin::start(github.public_assets());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let descriptor = zup_distribute_github::PackageDescriptor::parse(
            &github.public_assets()[&release.transport[0].descriptor],
            Some(release.transport[0].identity.digest),
        )
        .unwrap();
        assert!(descriptor.is_sharded());
        let layout = origin.pinned();
        let client = zup_distribute_github::client().unwrap();
        let package = zup_distribute_github::open(&layout, &descriptor, &client)
            .await
            .unwrap();
        let graph_path = release
            .documents
            .iter()
            .find(|d| d.path.starts_with("releases/"))
            .unwrap();
        let graph = zup_acquire::ReleaseDescriptor::parse(
            &std::fs::read(staged.web.as_ref().unwrap().join(&graph_path.path)).unwrap(),
        )
        .unwrap();
        let catalog = zup_acquire::ContentCatalog::parse(
            &std::fs::read(
                staged
                    .web
                    .as_ref()
                    .unwrap()
                    .join(zup_acquire::document_path(&graph.catalog.digest).to_string()),
            )
            .unwrap(),
        )
        .unwrap();
        let source =
            zup_distribute_github::GithubContentSource::new("release", &layout, package, catalog)
                .unwrap();
        let chain = zup_acquire::SourceChain::new(vec![std::sync::Arc::new(source)]);
        let cache_root = tempfile::tempdir().unwrap();
        let cache =
            zup_acquire::ContentCache::open(cache_root.path(), zup_acquire::CachePolicy::Keep)
                .unwrap();
        for object in &release.objects {
            let kind = zup_acquire::ContentKind::Runtime;
            let descriptor = zup_acquire::ContentDescriptor::of_document_kind(
                kind,
                object.identity.digest,
                object.identity.size,
            );
            let verified = chain
                .acquire(&descriptor, &cache, &zup_acquire::NeverCancelled)
                .await
                .unwrap();
            assert_eq!(
                zup_core::hash_bytes(&verified.read_to_end().unwrap()),
                object.identity.digest
            );
        }
    });
}
