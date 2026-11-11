use std::collections::BTreeMap;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tough::editor::{RepositoryEditor, signed::PathExists};
use tough::key_source::{KeySource, LocalKeySource};
use zup_acquire::{ContentCatalog, DocumentRef, ReleaseDescriptor, ReleaseVariant};
use zup_core::TargetBuildPlan;

pub const ROOT: &[u8] = include_bytes!("../../../zup-update/tests/fixtures/root.json");
const KEY: &[u8] = include_bytes!("../../../zup-update/tests/fixtures/signing.pem");

pub struct RepositoryServer {
    pub url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl RepositoryServer {
    pub fn start(directory: PathBuf) -> Self {
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
                stream.set_nonblocking(false).unwrap();
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
        let result = self.task.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

pub async fn publish(
    directory: &Path,
    plan: &TargetBuildPlan,
    metadata_version: u64,
    runtime_path: &Path,
    preset: Option<Vec<u8>>,
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
    #[cfg(windows)]
    let runtime = zup_windows::plan_only_runtime_bytes(runtime_path, plan, &[])
        .unwrap()
        .0;
    #[cfg(not(windows))]
    let runtime = std::fs::read(runtime_path).unwrap();
    let runtime_ref = DocumentRef::of(zup_core::hash_bytes(&runtime), runtime.len() as u64);
    let preset_descriptor = preset
        .as_deref()
        .map(|bytes| zup_artifact::Descriptor::of(zup_artifact::MediaType::PRESET, bytes));
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
        preset: preset_descriptor,
        requirements: Default::default(),
        plan: package.plan().clone(),
        logical_size: plan.total_size
            + runtime_ref.size
            + preset_descriptor.map_or(0, |preset| preset.size),
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let mut objects = BTreeMap::new();
    for file in &plan.files {
        objects.insert(file.sha256, std::fs::read(&file.source).unwrap());
    }
    objects.insert(runtime_ref.digest, runtime);
    if let Some(bytes) = preset {
        objects.insert(zup_core::hash_bytes(&bytes), bytes);
    }
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
            platform: zup_artifact::Platform::from_target(&plan.installer.target).to_string(),
            frontend: plan.installer.frontend.to_string(),
            manifest: DocumentRef::of(
                zup_core::hash_bytes(&manifest_bytes),
                manifest_bytes.len() as u64,
            ),
            runtime: Some(runtime_ref),
            preset: preset_descriptor.map(|preset| DocumentRef::of(preset.digest, preset.size)),
            content: manifest.content_digests(),
            requirements: Default::default(),
            logical_size: manifest.logical_size,
        }],
        downloads: Vec::new(),
    };
    release.release_digest = release.computed_digest().unwrap();
    let mut documents = vec![
        ("releases/stable.json".to_owned(), release.encode().unwrap()),
        (
            zup_acquire::document_path(&zup_core::hash_bytes(&release.encode().unwrap()))
                .to_string(),
            release.encode().unwrap(),
        ),
        (
            zup_acquire::document_path(&zup_core::hash_bytes(&catalog_bytes)).to_string(),
            catalog_bytes,
        ),
        (
            zup_acquire::document_path(&zup_core::hash_bytes(&manifest_bytes)).to_string(),
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
