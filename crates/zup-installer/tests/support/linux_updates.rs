use super::*;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use tough::editor::{RepositoryEditor, signed::PathExists};
use tough::key_source::{KeySource, LocalKeySource};
use zup_acquire::{ContentCatalog, DocumentRef, ReleaseDescriptor, ReleaseVariant};

const ROOT: &[u8] = include_bytes!("../../../zup-update/tests/fixtures/root.json");
const KEY: &[u8] = include_bytes!("../../../zup-update/tests/fixtures/signing.pem");

struct RepositoryServer {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl RepositoryServer {
    fn start(directory: PathBuf) -> Self {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let task = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let path = if path.starts_with("blobs/") {
                    directory.join("input").join(path)
                } else {
                    directory.join(path)
                };
                let bytes = std::fs::read(path);
                let (status, bytes) = match bytes {
                    Ok(bytes) => ("200 OK", bytes),
                    Err(_) => ("404 Not Found", Vec::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(&bytes);
            }
        });
        Self {
            url,
            stop,
            task: Some(task),
        }
    }
}
impl Drop for RepositoryServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.task.take().unwrap().join().unwrap();
    }
}

fn runtime(frontend: Frontend) -> &'static Path {
    Path::new(match frontend {
        Frontend::Console => env!("CARGO_BIN_EXE_zup-setup-console"),
        Frontend::Headless => env!("CARGO_BIN_EXE_zup-setup-headless"),
        _ => unreachable!(),
    })
}

fn compose(plan: &TargetBuildPlan, output: &Path) {
    let bytes = zup_bundle::BundleWriter::encode(plan, &[]).unwrap();
    zup_compose::compose(
        &plan.installer.target,
        runtime(plan.installer.frontend),
        output,
        &bytes,
        None,
    )
    .unwrap();
}

async fn publish(
    directory: &Path,
    plan: &TargetBuildPlan,
    metadata_version: u64,
) -> (ReleaseDescriptor, PathBuf) {
    let input = directory.join("input");
    let metadata = directory.join("metadata");
    let targets = directory.join("targets");
    let root = directory.join("root.json");
    let key = directory.join("signing.pem");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::create_dir_all(&targets).unwrap();
    std::fs::write(&root, ROOT).unwrap();
    std::fs::write(&key, KEY).unwrap();
    let package =
        zup_bundle::Package::parse(zup_bundle::BundleWriter::encode(plan, &[]).unwrap()).unwrap();
    let runtime = std::fs::read(runtime(plan.installer.frontend)).unwrap();
    let runtime_ref = DocumentRef::of(zup_core::hash_bytes(&runtime), runtime.len() as u64);
    let manifest = zup_artifact::VariantManifest {
        schema: zup_artifact::VARIANT_MANIFEST_SCHEMA,
        required_features: zup_artifact::FEATURE_VARIANT_MANIFESTS,
        target: plan.installer.target,
        platform: zup_artifact::Platform::from_target(&plan.installer.target),
        frontend: plan.installer.frontend,
        runtime: Some(zup_artifact::Descriptor::of(
            zup_artifact::MediaType::RUNTIME,
            &runtime,
        )),
        preset: None,
        requirements: Default::default(),
        plan: package.plan().clone(),
        logical_size: plan.total_size + runtime_ref.size,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let mut objects = BTreeMap::new();
    for file in &plan.files {
        objects.insert(file.sha256, std::fs::read(&file.source).unwrap());
    }
    objects.insert(runtime_ref.digest, runtime);
    let catalog = ContentCatalog::new(
        objects
            .iter()
            .map(|(digest, bytes)| zup_acquire::CatalogEntry::stored(*digest, bytes.len() as u64))
            .collect(),
    )
    .unwrap();
    let catalog_bytes = catalog.encode().unwrap();
    let mut release = ReleaseDescriptor {
        schema: zup_acquire::RELEASE_SCHEMA,
        app_id: plan.installer.app.id.clone(),
        channel: "stable".into(),
        version: plan.installer.app.version.to_string(),
        release_digest: zup_core::hash_bytes(b""),
        catalog: DocumentRef::of(
            zup_core::hash_bytes(&catalog_bytes),
            catalog_bytes.len() as u64,
        ),
        variants: vec![ReleaseVariant {
            id: plan.installer.target.to_string(),
            target: plan.installer.target,
            platform: "linux".into(),
            frontend: plan.installer.frontend.to_string(),
            manifest: DocumentRef::of(
                zup_core::hash_bytes(&manifest_bytes),
                manifest_bytes.len() as u64,
            ),
            runtime: Some(runtime_ref),
            preset: None,
            content: manifest.content_digests(),
            requirements: Default::default(),
            logical_size: manifest.logical_size,
        }],
        downloads: Vec::new(),
    };
    release.release_digest = release.computed_digest().unwrap();
    let mut documents = vec![
        ("releases/stable.json".to_owned(), release.encode().unwrap()),
        ("releases/stable/catalog.json".to_owned(), catalog_bytes),
        (
            format!("releases/stable/variants/{}.json", plan.installer.target),
            manifest_bytes,
        ),
    ];
    for (digest, bytes) in objects {
        let hex = digest.to_hex();
        documents.push((format!("blobs/sha256/{}/{}", &hex[..2], &hex[2..]), bytes));
    }
    let mut editor = RepositoryEditor::new(&root).await.unwrap();
    let version = NonZeroU64::new(metadata_version).unwrap();
    let expires = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24);
    editor.targets_version(version).unwrap();
    editor.targets_expires(expires).unwrap();
    editor.snapshot_version(version);
    editor.snapshot_expires(expires);
    editor.timestamp_version(version);
    editor.timestamp_expires(expires);
    for (relative, bytes) in &documents {
        let path = input.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        editor
            .add_target(
                relative.as_str(),
                tough::schema::Target::from_path(&path).await.unwrap(),
            )
            .unwrap();
    }
    let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource { path: key })];
    let signed = editor.sign(&keys).await.unwrap();
    signed.write(&metadata).await.unwrap();
    for (relative, _) in documents {
        signed
            .link_target(
                &input.join(&relative),
                &targets,
                PathExists::Replace,
                Some(&tough::TargetName::new(relative).unwrap()),
            )
            .await
            .unwrap();
    }
    (release, input)
}

#[test]
fn authenticated_updates_preserve_choices_and_support_offline_maintenance() {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for frontend in [Frontend::Headless, Frontend::Console] {
        let mut fixture = Fixture::new(frontend);
        let repository = fixture.root.path().join("repository");
        let server = RepositoryServer::start(repository.clone());
        fixture.plan.installer.updates = Some(zup_core::UpdateConfig {
            repository: server.url.clone(),
            channel: "stable".into(),
            trusted_root: ROOT.to_vec(),
        });
        compose(&fixture.plan, &fixture.setup);
        let install = fixture
            .command()
            .args([
                "install", "--scope", "user", "--enable", "docs", "--output", "json",
            ])
            .output()
            .unwrap();
        assert!(
            install.status.success(),
            "{}",
            String::from_utf8_lossy(&install.stdout)
        );
        let first_ledger = zup_linux::LinuxLedgerStore::new(fixture.root.path().join("state"))
            .load(
                &fixture.plan.installer.app.id,
                zup_core::SelectedScope::User,
            )
            .unwrap()
            .unwrap();
        let old_generation = zup_transaction::maintenance_directory(
            &fixture.root.path().join("state"),
            &first_ledger.app_id,
            first_ledger.scope,
            &first_ledger.version,
        );
        let foreign = old_generation.join("maintenance.foreign");
        std::fs::write(&foreign, b"keep this file").unwrap();
        fixture.plan.installer.app.version = "2.0.0".parse().unwrap();
        let core = fixture
            .plan
            .files
            .iter_mut()
            .find(|file| file.component.as_ref().unwrap().as_str() == "core")
            .unwrap();
        std::fs::write(&core.source, b"updated core").unwrap();
        core.size = 12;
        core.sha256 = zup_core::hash_bytes(b"updated core");
        fixture.plan.total_size = fixture.plan.files.iter().map(|file| file.size).sum();
        let (release, seed) = tokio.block_on(publish(&repository, &fixture.plan, 1));
        let check = fixture
            .command()
            .args(["update", "check", "--scope", "user", "--output", "json"])
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "{} {}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr)
        );
        let result: Value = serde_json::from_slice(&check.stdout).unwrap();
        assert!(result["message"].as_str().unwrap().contains("2.0.0"));
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"core"
        );
        if frontend == Frontend::Headless {
            use std::io::{BufRead, BufReader};
            let mut interrupted = fixture
                .command()
                .args([
                    "update", "--scope", "user", "--yes", "--output", "jsonl", "--source",
                ])
                .arg(&seed)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = BufReader::new(interrupted.stdout.take().unwrap());
            loop {
                let mut line = String::new();
                assert!(
                    stdout.read_line(&mut line).unwrap() > 0,
                    "acquisition must report progress before finishing"
                );
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["type"] == "progress" {
                    break;
                }
            }
            assert!(
                Command::new("kill")
                    .args(["-INT", &interrupted.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let mut remainder = String::new();
            stdout.read_to_string(&mut remainder).unwrap();
            assert_eq!(interrupted.wait().unwrap().code(), Some(2), "{remainder}");
            assert_eq!(
                std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
                b"core"
            );
        }
        let digest = fixture
            .plan
            .files
            .iter()
            .find(|file| file.component.as_ref().unwrap().as_str() == "core")
            .unwrap()
            .sha256
            .to_hex();
        let blob = seed.join(format!("blobs/sha256/{}/{}", &digest[..2], &digest[2..]));
        zup_acquire::ContentCache::open(
            fixture.root.path().join("state/content"),
            zup_acquire::CachePolicy::Auto,
        )
        .unwrap()
        .forget_digest(&digest.parse::<zup_core::Sha256Digest>().unwrap())
        .unwrap();
        std::fs::write(&blob, b"hostile core").unwrap();
        let refused = fixture
            .command()
            .args([
                "update", "--scope", "user", "--yes", "--output", "json", "--source",
            ])
            .arg(&seed)
            .output()
            .unwrap();
        assert!(!refused.status.success());
        serde_json::from_slice::<Value>(&refused.stdout).unwrap();
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"core"
        );
        std::fs::write(&blob, b"updated core").unwrap();
        if frontend == Frontend::Console {
            use zup_session::SessionHost;
            let host = zup_linux::LinuxSessionHost::open(fixture.setup.clone());
            let context = host
                .observe(
                    &fixture.plan.installer,
                    zup_core::SelectedScope::User,
                    Some(&fixture.root.path().join("state")),
                )
                .unwrap();
            let mut states = Vec::new();
            let outcome = host
                .update(
                    &context,
                    &zup_runtime::UpdateRequest::Install {
                        version: Some("2.0.0".into()),
                    },
                    &zup_runtime::OperationControl::new(
                        zup_runtime::ExecutionPolicy::NonInteractive,
                    ),
                    &mut |state| states.push(state),
                )
                .unwrap();
            assert!(matches!(outcome, zup_runtime::InstallOutcome::Committed));
            assert!(!states.is_empty());
        }
        let update = fixture
            .command()
            .args([
                "update", "--scope", "user", "--yes", "--output", "jsonl", "--source",
            ])
            .arg(&seed)
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{} {}",
            String::from_utf8_lossy(&update.stdout),
            String::from_utf8_lossy(&update.stderr)
        );
        let events: Vec<Value> = String::from_utf8(update.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "completed")
                .count(),
            1
        );
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"updated core"
        );
        assert!(fixture.root.path().join("application/docs.txt").exists());
        let state = fixture.root.path().join("state");
        let ledger = zup_linux::LinuxLedgerStore::new(&state)
            .load(
                &fixture.plan.installer.app.id,
                zup_core::SelectedScope::User,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            ledger.release_identity().unwrap().release,
            release.release_digest
        );
        assert!(
            ledger
                .selected_components
                .iter()
                .any(|id| id.as_str() == "docs")
        );
        let maintenance = zup_transaction::maintenance_runtime_path(
            &state,
            &ledger.app_id,
            ledger.scope,
            &ledger.version,
            "",
        );
        for transaction in [
            &first_ledger.committed_transaction,
            &ledger.committed_transaction,
            &ledger.committed_transaction,
        ] {
            let recovered = fixture
                .command()
                .args([
                    "recover",
                    "--transaction",
                    transaction,
                    "--scope",
                    "user",
                    "--output",
                    "json",
                ])
                .output()
                .unwrap();
            assert!(
                recovered.status.success(),
                "{} {}",
                String::from_utf8_lossy(&recovered.stdout),
                String::from_utf8_lossy(&recovered.stderr)
            );
            serde_json::from_slice::<Value>(&recovered.stdout).unwrap();
        }
        assert_eq!(std::fs::read(&foreign).unwrap(), b"keep this file");
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"updated core"
        );
        std::fs::remove_dir_all(&repository).unwrap();
        std::fs::write(fixture.root.path().join("application/core.txt"), b"drifted").unwrap();
        for args in [
            vec!["repair", "--force-files"],
            vec!["modify", "--disable", "docs"],
            vec!["uninstall"],
        ] {
            let output = Command::new(&maintenance)
                .args(args)
                .args(["--scope", "user", "--output", "json", "--state-root"])
                .arg(&state)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap();
        }
        assert!(!fixture.root.path().join("application/core.txt").exists());
    }
}
