#![cfg(windows)]

use std::{num::NonZeroU64, path::Path};
use tough::{
    TargetName,
    editor::{RepositoryEditor, signed::PathExists},
    key_source::{KeySource, LocalKeySource},
    schema::Target,
};
use zup_acquire::{ContentCatalog, DocumentRef, ReleaseDescriptor, ReleaseVariant, RuntimeHandoff};
use zup_core::ReleasePin;
use zup_core::{Frontend, hash_bytes};

#[path = "support/project.rs"]
mod project;

const ROOT: &[u8] = include_bytes!("../../zup-update/tests/fixtures/root.json");
const KEY: &[u8] = include_bytes!("../../zup-update/tests/fixtures/signing.pem");

async fn sign(directory: &Path, documents: &[(String, Vec<u8>)], metadata_version: u64) {
    let root = directory.join("root.json");
    let key = directory.join("signing.pem");
    std::fs::write(&root, ROOT).unwrap();
    std::fs::write(&key, KEY).unwrap();
    let mut editor = RepositoryEditor::new(&root).await.unwrap();
    let version = NonZeroU64::new(metadata_version).unwrap();
    let expires = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24);
    editor.targets_version(version).unwrap();
    editor.targets_expires(expires).unwrap();
    editor.snapshot_version(version);
    editor.snapshot_expires(expires);
    editor.timestamp_version(version);
    editor.timestamp_expires(expires);
    std::fs::create_dir_all(directory.join("targets")).unwrap();
    let input = directory.join("input");
    for (name, bytes) in documents {
        let path = input.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    let mut files = Vec::new();
    let mut pending = vec![input.clone()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    for path in &files {
        let name = path
            .strip_prefix(&input)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        editor
            .add_target(name.as_str(), Target::from_path(path).await.unwrap())
            .unwrap();
    }
    let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource { path: key })];
    let signed = editor.sign(&keys).await.unwrap();
    signed.write(directory.join("metadata")).await.unwrap();
    for path in files {
        let name = path
            .strip_prefix(&input)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        signed
            .link_target(
                &path,
                &directory.join("targets"),
                PathExists::Replace,
                Some(&TargetName::new(name).unwrap()),
            )
            .await
            .unwrap();
    }
}

struct ReleaseFixture {
    root: tempfile::TempDir,
    app: project::AppSpec,
    build: zup_core::TargetBuildPlan,
    published: PublishedRelease,
}

struct PublishedRelease {
    runtime_path: std::path::PathBuf,
    runtime: Vec<u8>,
    manifest: zup_artifact::VariantManifest,
    handoff: RuntimeHandoff,
    content: zup_windows::HandoffContent,
}

async fn release_fixture(embedded_repository: Option<&str>) -> ReleaseFixture {
    let root = tempfile::tempdir_in(zup_windows::user_data().unwrap()).unwrap();
    let app = project::AppSpec::unique("handoff-trust");
    let mut build = project::plan(
        &app,
        Frontend::Console,
        &[project::Payload::named("payload.bin", b"payload", None)],
        &[],
        root.path(),
    );
    build.files[0].placement = zup_core::PayloadPlacement::Generation {
        path: zup_core::RelativePath::new("payload.bin").unwrap(),
    };
    build.installer.files[0].placement = zup_core::PayloadMapping::GenerationFile {
        path: zup_core::RelativePath::new("payload.bin").unwrap(),
    };
    let repository = format!(
        "file:///{}",
        root.path().display().to_string().replace('\\', "/")
    );
    build.installer.updates = Some(zup_core::UpdateConfig {
        repository: embedded_repository.unwrap_or(&repository).into(),
        channel: "stable".into(),
        trusted_root: ROOT.to_vec(),
    });
    let published = publish(root.path(), &build, 1).await;
    ReleaseFixture {
        root,
        app,
        build,
        published,
    }
}

async fn publish(
    directory: &Path,
    build: &zup_core::TargetBuildPlan,
    metadata_version: u64,
) -> PublishedRelease {
    let (runtime, _) =
        zup_windows::plan_only_runtime_bytes(&project::template(Frontend::Console), build, &[])
            .unwrap();
    let runtime_path = directory.join("runtime.exe");
    std::fs::write(&runtime_path, &runtime).unwrap();
    let embedded = zup_windows::EmbeddedBundle::open(&runtime_path).unwrap();
    let runtime_descriptor =
        zup_artifact::Descriptor::of(zup_artifact::MediaType::RUNTIME, &runtime);
    let manifest = zup_artifact::VariantManifest {
        schema: zup_artifact::VARIANT_MANIFEST_SCHEMA,
        required_features: zup_artifact::FEATURE_VARIANT_MANIFESTS,
        target: build.installer.target,
        platform: zup_artifact::Platform::from_target(&build.installer.target),
        frontend: Frontend::Console,
        runtime: Some(runtime_descriptor),
        preset: None,
        requirements: Default::default(),
        plan: embedded.package().plan().clone(),
        logical_size: runtime.len() as u64 + build.total_size,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let mut entries = vec![
        zup_acquire::CatalogEntry::stored(runtime_descriptor.digest, runtime.len() as u64),
        zup_acquire::CatalogEntry::stored(build.files[0].sha256, build.files[0].size),
    ];
    entries.sort_by_key(|entry| entry.digest);
    let catalog = ContentCatalog::new(entries).unwrap();
    let catalog_bytes = catalog.encode().unwrap();
    let mut release = ReleaseDescriptor {
        schema: zup_acquire::RELEASE_SCHEMA,
        app_id: build.installer.app.id.clone(),
        channel: "stable".into(),
        version: build.installer.app.version.to_string(),
        release_digest: hash_bytes(b""),
        catalog: DocumentRef::of(hash_bytes(&catalog_bytes), catalog_bytes.len() as u64),
        variants: vec![ReleaseVariant {
            id: "application".into(),
            target: build.installer.target,
            platform: "windows".into(),
            frontend: "console".into(),
            manifest: DocumentRef::of(hash_bytes(&manifest_bytes), manifest_bytes.len() as u64),
            runtime: Some(DocumentRef::of(
                runtime_descriptor.digest,
                runtime_descriptor.size,
            )),
            preset: None,
            content: manifest.content_digests(),
            requirements: Default::default(),
            logical_size: runtime.len() as u64 + build.total_size,
        }],
        downloads: Vec::new(),
    };
    release.release_digest = release.computed_digest().unwrap();
    let release_bytes = release.encode().unwrap();
    sign(
        directory,
        &[
            ("releases/stable.json".into(), release_bytes.clone()),
            (
                zup_acquire::document_path(&hash_bytes(&release_bytes)).to_string(),
                release_bytes.clone(),
            ),
            (
                zup_acquire::document_path(&hash_bytes(&catalog_bytes)).to_string(),
                catalog_bytes,
            ),
            (
                zup_acquire::document_path(&hash_bytes(&manifest_bytes)).to_string(),
                manifest_bytes.clone(),
            ),
        ],
        metadata_version,
    )
    .await;
    let handoff = RuntimeHandoff {
        repository: format!(
            "file:///{}",
            directory.display().to_string().replace('\\', "/")
        ),
        pin: ReleasePin::Channel {
            channel: "stable".into(),
        },
        schema: zup_acquire::HANDOFF_SCHEMA,
        app_id: release.app_id.clone(),
        release: release.release_digest,
        document: hash_bytes(&release_bytes),
        catalog: release.catalog.digest,
        variant: "application".into(),
        manifest: hash_bytes(&manifest_bytes),
        runtime: runtime_descriptor.digest,
        target: build.installer.target,
        frontend: "console".into(),
        mode: zup_acquire::HandoffMode::Install,
        scope: "user".into(),
        session: Default::default(),
        components: Vec::new(),
    };
    let content = zup_windows::HandoffContent {
        handoff: handoff.clone(),
        release,
        catalog,
        manifest: manifest_bytes,
    };
    let payload_bytes = std::fs::read(&build.files[0].source).unwrap();
    for (digest, bytes) in [
        (runtime_descriptor.digest, runtime.as_slice()),
        (build.files[0].sha256, payload_bytes.as_slice()),
    ] {
        let path = directory.join(zup_acquire::blob_path(&digest).to_string());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    PublishedRelease {
        runtime_path,
        runtime,
        manifest,
        handoff,
        content,
    }
}

#[tokio::test]
async fn runtime_handoff_requires_tuf_authorization_under_its_embedded_root() {
    let ReleaseFixture {
        root,
        build,
        published:
            PublishedRelease {
                runtime_path,
                runtime,
                manifest,
                handoff,
                content,
            },
        ..
    } = release_fixture(Some("https://unused.invalid")).await;
    let bundle = zup_windows::AcquiredBundle::open(&runtime_path, &content).unwrap();
    let resolved = bundle
        .authenticate(&handoff, &root.path().join("state"))
        .await
        .unwrap();
    assert_eq!(resolved.trust_anchor, hash_bytes(ROOT));
    assert!(!resolved.pin.is_pinned());
    assert_eq!(hash_bytes(&resolved.descriptor_bytes), handoff.document);

    let cache = std::sync::Arc::new(
        zup_acquire::ContentCache::open(root.path().join("cache"), zup_acquire::CachePolicy::Keep)
            .unwrap(),
    );
    let descriptor = resolved.require_runtime().unwrap();
    let mut writer = cache.writer(&descriptor).unwrap();
    writer.write(&runtime).unwrap();
    writer.commit().unwrap();
    let acquired = zup_update::content::Acquired {
        resolved: resolved.clone(),
        manifest: manifest.clone(),
        plan: zup_acquire::AcquisitionPlan::empty(),
        cache,
        outcome: zup_acquire::AcquisitionOutcome {
            items: Vec::new(),
            estimate: Default::default(),
            elapsed: Default::default(),
            cache_hits: 0,
            total_bytes: 0,
        },
    };
    let image = zup_windows::AcquiredRuntime::stage(&acquired).unwrap();
    assert_eq!(std::fs::read(image.path()).unwrap(), runtime);
    assert_eq!(
        zup_windows::EmbeddedBundle::open(image.path())
            .unwrap()
            .plan(),
        &manifest.plan
    );
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(image.path())
            .is_err()
    );
    assert!(std::fs::remove_file(image.path()).is_err());
    let directory = image.path().parent().unwrap();
    assert!(std::fs::rename(directory, directory.with_file_name("redirected")).is_err());

    let identity = resolved.identity(&build.installer.components);
    let timestamp = root.path().join("metadata/timestamp.json");
    let mut invalid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&timestamp).unwrap()).unwrap();
    invalid["signatures"][0]["sig"] = serde_json::Value::String("00".repeat(64));
    std::fs::write(timestamp, serde_json::to_vec(&invalid).unwrap()).unwrap();
    assert!(
        bundle
            .authenticate(&handoff, &root.path().join("state"))
            .await
            .is_err()
    );
    bundle
        .validate_recorded_source(&content, &identity)
        .unwrap();
    let mut foreign = identity;
    foreign.manifest = hash_bytes(b"another manifest");
    assert!(bundle.validate_recorded_source(&content, &foreign).is_err());
}

fn install_previous(
    fixture: &ReleaseFixture,
    state: &project::State,
    template: &Path,
) -> std::path::PathBuf {
    let mut previous = fixture.app.clone();
    previous.version = "0.9.0".into();
    let source = fixture.root.path().join("previous");
    std::fs::create_dir(&source).unwrap();
    let mut plan = project::plan(
        &previous,
        Frontend::Console,
        &[project::Payload::named(
            "payload.bin",
            b"previous payload",
            None,
        )],
        &[],
        &source,
    );
    plan.files[0].placement = zup_core::PayloadPlacement::Generation {
        path: zup_core::RelativePath::new("payload.bin").unwrap(),
    };
    plan.installer.files[0].placement = fixture.build.installer.files[0].placement.clone();
    plan.installer.updates = fixture.build.installer.updates.clone();
    let setup = source.join("Setup.exe");
    zup_windows::build_self_contained_executable(template, &setup, &plan, &[], None).unwrap();
    project::succeed(&setup, state, "install", &["--non-interactive", "--yes"]);
    state.maintenance(&previous)
}

#[tokio::test]
async fn maintenance_keeps_exact_sources_for_repair_and_follows_the_channel_for_updates() {
    let fixture = release_fixture(None).await;
    let state = project::State::new();
    let _cleanup = project::cleanup(&fixture.app, &state);
    let old_tool = install_previous(&fixture, &state, &project::template(Frontend::Console));
    let before = std::fs::read(&old_tool).unwrap();
    project::succeed(
        &old_tool,
        &state,
        "update",
        &[
            "--non-interactive",
            "--yes",
            "--source",
            fixture.root.path().to_str().unwrap(),
        ],
    );
    let store = zup_windows::InstallationRecordStore::new(state.path());
    let load = || {
        store
            .load(
                &fixture.build.installer.app.id,
                zup_core::SelectedScope::User,
            )
            .unwrap()
            .unwrap()
    };
    let payload_path = |record: &zup_plan::InstallationRecord| {
        std::path::Path::new(
            record
                .current
                .materialization
                .as_ref()
                .unwrap()
                .directory()
                .as_str(),
        )
        .join("payload.bin")
    };
    let record = load();
    assert_eq!(record.current.version.to_string(), "1.0.0");
    assert_eq!(
        std::fs::read(state.maintenance(&fixture.app)).unwrap(),
        fixture.published.runtime
    );
    assert_eq!(std::fs::read(old_tool).unwrap(), before);
    let payload = payload_path(&record);
    assert_eq!(std::fs::read(&payload).unwrap(), b"payload");
    let original = record.release_identity().unwrap().clone();
    let future = fixture.root.path().join("future");
    std::fs::create_dir(&future).unwrap();
    let mut next = fixture.build.clone();
    next.files[0].source = future.join("payload.bin");
    std::fs::write(&next.files[0].source, b"next payload").unwrap();
    next.files[0].size = 12;
    next.files[0].sha256 = hash_bytes(b"next payload");
    next.total_size = 12;
    let published = publish(fixture.root.path(), &next, 2).await;
    assert_ne!(published.handoff.document, original.document);
    let cache = zup_acquire::ContentCache::open(
        state.path().join("content"),
        zup_acquire::CachePolicy::Keep,
    )
    .unwrap();
    cache.forget_digest(&original.runtime.unwrap()).unwrap();
    cache.forget_digest(&fixture.build.files[0].sha256).unwrap();
    std::fs::write(&payload, b"damaged").unwrap();
    project::succeed(
        &state.maintenance(&fixture.app),
        &state,
        "repair",
        &["--non-interactive", "--yes"],
    );
    let repaired = load();
    repaired
        .release_identity()
        .unwrap()
        .verify_source(&original)
        .unwrap();
    assert_eq!(std::fs::read(payload_path(&repaired)).unwrap(), b"payload");
    project::succeed(
        &state.maintenance(&fixture.app),
        &state,
        "modify",
        &["--non-interactive", "--yes"],
    );
    load()
        .release_identity()
        .unwrap()
        .verify_source(&original)
        .unwrap();
    project::succeed(
        &state.maintenance(&fixture.app),
        &state,
        "update",
        &["--non-interactive", "--yes"],
    );
    let updated = load();
    assert_eq!(updated.current.version, repaired.current.version);
    assert_eq!(
        updated.release_identity().unwrap().document,
        published.handoff.document
    );
    assert_eq!(
        std::fs::read(payload_path(&updated)).unwrap(),
        b"next payload"
    );
    assert_eq!(
        std::fs::read(state.maintenance(&fixture.app)).unwrap(),
        published.runtime
    );
    project::succeed(
        &state.maintenance(&fixture.app),
        &state,
        "uninstall",
        &["--non-interactive", "--yes"],
    );
}

#[cfg(all(feature = "test-faults", feature = "console"))]
#[tokio::test]
async fn authenticated_acquired_recovery_survives_process_exit_without_cache_or_repository() {
    let fixture = release_fixture(None).await;
    let state = project::State::new();
    let _cleanup = project::cleanup(&fixture.app, &state);
    let old_tool = install_previous(
        &fixture,
        &state,
        Path::new(env!("CARGO_BIN_EXE_zup-setup-console")),
    );
    let interrupted = std::process::Command::new(&old_tool)
        .args(["update", "--scope", "user", "--state-root"])
        .arg(state.path())
        .args(["--non-interactive", "--yes", "--source"])
        .arg(fixture.root.path())
        .env("ZUP_TEST_FAULT_POINT", "apply_returned")
        .env("ZUP_TEST_FAULT_OPERATION", "generation:activate:")
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    use zup_transaction::TransactionStore;
    let store = zup_transaction::FilesystemTransactionStore::private_at(state.path());
    let pending = store
        .list_ids()
        .unwrap()
        .into_iter()
        .map(|id| store.load(&id).unwrap())
        .find(|record| record.app_version.to_string() == fixture.app.version)
        .unwrap_or_else(|| {
            panic!(
                "no acquired transaction: {} {}",
                String::from_utf8_lossy(&interrupted.stdout),
                String::from_utf8_lossy(&interrupted.stderr)
            )
        });
    let release = pending
        .release
        .clone()
        .expect("authenticated release retained in journal");
    assert_eq!(release.trust_anchor, hash_bytes(ROOT));
    assert_eq!(release.document, fixture.published.handoff.document);
    let retained = state
        .path()
        .join("transactions")
        .join(pending.transaction_id.to_string())
        .join("recovery");
    assert!(retained.join("source.json").exists());
    std::fs::remove_dir_all(state.path().join("content")).unwrap();
    std::fs::remove_dir_all(fixture.root.path()).unwrap();
    project::succeed(
        &old_tool,
        &state,
        "recover",
        &["--transaction", &pending.transaction_id.to_string()],
    );
    let installed = zup_windows::InstallationRecordStore::new(state.path())
        .load(
            &fixture.build.installer.app.id,
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    assert_eq!(installed.current.version.to_string(), fixture.app.version);
    assert_eq!(installed.release_identity(), Some(&release));
    let materialization = installed.current.materialization.unwrap();
    let directory = materialization.directory();
    assert_eq!(
        std::fs::read(Path::new(directory.as_str()).join("payload.bin")).unwrap(),
        b"payload"
    );
    assert!(!retained.exists());
}

#[tokio::test]
#[ignore = "exports a signed acquired release for the isolated Windows recovery VM"]
async fn export_machine_acquired_recovery_fixture() {
    let output = std::path::PathBuf::from(
        std::env::var_os("ZUP_ACQUIRED_RECOVERY_FIXTURE_ROOT").expect("fixture output directory"),
    );
    std::fs::create_dir_all(&output).unwrap();
    let source = tempfile::tempdir().unwrap();
    let app = project::AppSpec::unique("machine-acquired").at_version("2.0.0");
    let folder = format!("ZupF05Acquired-{}", uuid::Uuid::now_v7().simple());
    let machine_plan = |version: &str, bytes: &[u8], directory: &Path| {
        std::fs::create_dir_all(directory).unwrap();
        let mut plan = project::plan(
            &app.at_version(version),
            Frontend::Console,
            &[project::Payload::named("payload.bin", bytes, None)],
            &[],
            directory,
        );
        plan.installer.install.scope = zup_core::InstallScope::Machine;
        plan.installer.install.directory = zup_core::InstallDirectory {
            user: None,
            machine: Some(
                zup_core::Template::parse(&format!("${{location.programs}}/{folder}")).unwrap(),
            ),
        };
        plan.installer.updates = Some(zup_core::UpdateConfig {
            repository: "file:///C:/ZupF05Acquired/repository".into(),
            channel: "stable".into(),
            trusted_root: ROOT.to_vec(),
        });
        plan.files[0].placement = zup_core::PayloadPlacement::Generation {
            path: zup_core::RelativePath::new("payload.bin").unwrap(),
        };
        plan.installer.files[0].placement = zup_core::PayloadMapping::GenerationFile {
            path: zup_core::RelativePath::new("payload.bin").unwrap(),
        };
        plan
    };
    let candidate = machine_plan("2.0.0", b"2.0.0", &source.path().join("next"));
    let repository = output.join("repository");
    std::fs::create_dir_all(&repository).unwrap();
    let published = publish(&repository, &candidate, 1).await;
    let original = machine_plan("1.0.0", b"1.0.0", &source.path().join("old"));
    zup_windows::build_self_contained_executable(
        &project::template(Frontend::Console),
        &output.join("setup-1.0.0.exe"),
        &original,
        &[],
        None,
    )
    .unwrap();
    std::fs::write(
        output.join("fixture.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "app": app.id,
            "install_folder": folder,
            "trust_anchor": hash_bytes(ROOT),
            "release": published.handoff.release,
        }))
        .unwrap(),
    )
    .unwrap();
}
