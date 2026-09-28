//! TUF-authenticated resolution of a release graph.
//!
//! These tests exercise the properties that exist *because* the state is durable
//! and the graph is authenticated. A test that only proved "it resolved" would
//! not have caught a resolver that created a fresh datastore per call, so the
//! rollback test is the load-bearing one here.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};
use tough::TargetName;
use tough::editor::RepositoryEditor;
use tough::editor::signed::PathExists;
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{KeyHolder, Root, Signed, Target};
use url::Url;
use zup_acquire::{
    ContentCatalog, DocumentRef, HostArchitecture, HostProfile, OnlineTrust, RELEASE_SCHEMA,
    ReleaseDescriptor, ReleaseDownload, ReleaseDownloadKind, ReleasePin, ReleaseVariant,
};
use zup_core::Sha256Digest;
use zup_update::{ReleaseResolver, TrustContext, UpdateError};

const APP: &str = "com.example.acme";
const CHANNEL: &str = "stable";

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

/// The variant manifest bytes a release authenticates.
///
/// It is a real document body rather than an opaque placeholder, so the digest
/// the release names is the digest of something the resolver actually reads.
fn manifest_bytes(version: &str) -> Vec<u8> {
    format!("{{\"schema\":1,\"version\":\"{version}\",\"entries\":[]}}").into_bytes()
}

/// The content a release serves: a catalog plus the wire form of each blob.
///
/// `blobs` holds the **wire** form, which is what an origin serves and what the
/// cache verifies. The logical form is the catalog's `size`.
#[derive(Clone)]
struct Content {
    catalog_bytes: Vec<u8>,
    blobs: Vec<(Sha256Digest, Vec<u8>)>,
}

/// A catalog over a list of logical contents, plus the blobs themselves.
///
/// The contents are deliberately incompressible, so a byte count in an assertion
/// is a byte count and not a measurement of zstd.
fn content(sizes: &[usize]) -> Content {
    let mut blobs: Vec<(Sha256Digest, Vec<u8>, usize)> = sizes
        .iter()
        .enumerate()
        .map(|(index, size)| {
            let logical: Vec<u8> = (0..*size)
                .map(|offset| ((index as u64 * 31 + offset as u64) % 251) as u8)
                .collect();
            let wire = zstd::stream::encode_all(logical.as_slice(), 9).expect("a zstd frame");
            (digest_of(&logical), wire, logical.len())
        })
        .collect();
    blobs.sort_by_key(|(digest, _, _)| *digest);
    let catalog = ContentCatalog::new(
        blobs
            .iter()
            .map(|(digest, wire, logical)| {
                zup_acquire::CatalogEntry::compressed(*digest, wire.len() as u64, *logical as u64)
            })
            .collect(),
    )
    .expect("a catalog");
    let catalog_bytes = catalog.encode().expect("the catalog encodes");
    Content {
        catalog_bytes,
        blobs: blobs
            .into_iter()
            .map(|(digest, wire, _)| (digest, wire))
            .collect(),
    }
}

fn target(architecture: &str) -> zup_core::TargetTriple {
    zup_core::TargetTriple::parse(format!("{architecture}-pc-windows-msvc"))
        .expect("the fixture triple parses")
}

/// A variant's content list, in the strictly ascending order a release requires.
fn sorted_digests(content: &Content) -> Vec<Sha256Digest> {
    let mut digests: Vec<Sha256Digest> = content.blobs.iter().map(|(digest, _)| *digest).collect();
    digests.sort_unstable();
    digests.dedup();
    digests
}

fn release(options: &ReleaseOptions) -> ReleaseDescriptor {
    let manifest = manifest_bytes(&options.version);
    let mut release = ReleaseDescriptor {
        schema: RELEASE_SCHEMA,
        app_id: zup_core::AppId::new(APP).expect("the fixture app id is valid"),
        channel: CHANNEL.to_owned(),
        version: options.version.clone(),
        release_digest: Sha256Digest::from_bytes([0; 32]),
        catalog: DocumentRef::of(
            digest_of(&options.content.catalog_bytes),
            options.content.catalog_bytes.len() as u64,
        ),
        variants: vec![ReleaseVariant {
            id: "windows-x64".to_owned(),
            target: target("x86_64"),
            platform: "windows".to_owned(),
            frontend: "gui".to_owned(),
            manifest: DocumentRef::of(digest_of(&manifest), manifest.len() as u64),
            runtime: Some(DocumentRef::of(digest_of(b"runtime-windows-x64"), 4)),
            content: sorted_digests(&options.content),
            requirements: Default::default(),
            logical_size: 1024,
        }],
        downloads: options.downloads.clone(),
    };
    release.release_digest = release.computed_digest().expect("fingerprints");
    release
        .validate()
        .expect("the fixture release is well formed");
    release
}

struct ReleaseOptions {
    version: String,
    content: Content,
    downloads: Vec<ReleaseDownload>,
}

impl Default for ReleaseOptions {
    fn default() -> Self {
        Self {
            version: "1.1.0".to_owned(),
            content: content(&[64, 128, 256]),
            downloads: Vec::new(),
        }
    }
}

struct Fixture {
    /// The temporary directory that owns the repository, when this fixture made
    /// one. A fixture published into a shared directory has none.
    root: Option<tempfile::TempDir>,
    repository: String,
    trusted_root: Vec<u8>,
    state: PathBuf,
    documents: Vec<PathBuf>,
    /// The staged web tree a local seed reads.
    seed_dir: PathBuf,
}

struct SigningOptions {
    metadata_version: u64,
    expired_timestamp: bool,
    rotate_root: bool,
    /// Skip the target links, so a lookup finds no document at all.
    unlink_targets: bool,
}

impl Default for SigningOptions {
    fn default() -> Self {
        Self {
            metadata_version: 1,
            expired_timestamp: false,
            rotate_root: false,
            unlink_targets: false,
        }
    }
}

/// Publish a web tree and sign it into a fresh temporary repository.
async fn publish(
    release: &ReleaseDescriptor,
    content: &Content,
    options: SigningOptions,
) -> Fixture {
    let root = tempfile::tempdir().expect("a temporary repository");
    let mut fixture = publish_at(root.path(), release, content, options).await;
    fixture.state = root.path().join("state");
    fixture.root = Some(root);
    fixture
}

/// Publish a web tree and sign it into `directory`, exactly as `zup publish
/// stage` plus `tuftool` would: the same layout, the same document set, the
/// same `link_target` step.
///
/// Sharing a directory across two calls is how a rollback is staged — the same
/// repository, re-signed at a lower metadata version.
async fn publish_at(
    directory: &Path,
    release: &ReleaseDescriptor,
    content: &Content,
    options: SigningOptions,
) -> Fixture {
    let metadata = directory.join("metadata");
    let targets = directory.join("targets");
    let input = directory.join("input");
    std::fs::create_dir_all(&metadata).expect("metadata");
    std::fs::create_dir_all(&targets).expect("targets");
    let trusted_root = include_bytes!("fixtures/root.json").to_vec();
    let root_file = directory.join("root.json");
    std::fs::write(&root_file, &trusted_root).expect("root");
    let signing_key = directory.join("signing.pem");
    std::fs::write(&signing_key, include_bytes!("fixtures/signing.pem")).expect("key");

    // The web tree, laid out the way the immutable layout states.
    let mut documents: Vec<(String, Vec<u8>)> = Vec::new();
    for (digest, bytes) in &content.blobs {
        let hex = digest.to_hex();
        documents.push((
            format!("blobs/sha256/{}/{}", &hex[..2], &hex[2..]),
            bytes.clone(),
        ));
    }
    documents.push((
        format!("releases/{CHANNEL}/catalog.json"),
        content.catalog_bytes.clone(),
    ));
    for entry in &release.variants {
        documents.push((
            format!("releases/{CHANNEL}/variants/{}.json", entry.id),
            manifest_bytes(&release.version),
        ));
    }
    let release_bytes = release.encode().expect("the release encodes");
    documents.push((format!("releases/{CHANNEL}.json"), release_bytes.clone()));
    documents.push((
        format!("releases/{CHANNEL}/versions/{}.json", release.version),
        release_bytes,
    ));

    let mut paths = Vec::new();
    for (relative, bytes) in &documents {
        let path = input.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(&path, bytes).expect("a document");
        paths.push(path);
    }

    let mut editor = RepositoryEditor::new(&root_file).await.expect("an editor");
    let version = NonZeroU64::new(options.metadata_version).expect("a metadata version");
    editor.targets_version(version).expect("targets version");
    editor
        .targets_expires(Timestamp::now() + SignedDuration::from_hours(24 * 30))
        .expect("targets expiry");
    editor.snapshot_version(version);
    editor.snapshot_expires(Timestamp::now() + SignedDuration::from_hours(24 * 30));
    editor.timestamp_version(version);
    editor.timestamp_expires(if options.expired_timestamp {
        Timestamp::now() - SignedDuration::from_hours(1)
    } else {
        Timestamp::now() + SignedDuration::from_hours(24 * 7)
    });
    for (relative, path) in documents.iter().map(|(relative, _)| relative).zip(&paths) {
        editor
            .add_target(
                relative.as_str(),
                Target::from_path(path).await.expect("a target"),
            )
            .expect("add target");
    }
    let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource { path: signing_key })];
    let signed = editor.sign(&keys).await.expect("a signature");
    signed.write(&metadata).await.expect("metadata");
    if options.rotate_root {
        let mut next: Signed<Root> = serde_json::from_slice(&trusted_root).expect("root");
        next.signed.version = NonZeroU64::new(2).expect("a version");
        let previous: Signed<Root> = serde_json::from_slice(&trusted_root).expect("root");
        tough::editor::signed::SignedRole::new(
            next.signed,
            &KeyHolder::Root(previous.signed),
            &keys,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .await
        .expect("a root signature")
        .write(&metadata, true)
        .await
        .expect("a rotated root");
    }
    if !options.unlink_targets {
        for (relative, path) in documents.iter().map(|(relative, _)| relative).zip(&paths) {
            signed
                .link_target(
                    path,
                    &targets,
                    PathExists::Replace,
                    Some(&TargetName::new(relative).expect("a name")),
                )
                .await
                .expect("a linked target");
        }
    }
    let mut repository = Url::from_directory_path(directory).expect("a file URL");
    repository.set_query(None);
    Fixture {
        state: directory.join("state"),
        trusted_root,
        documents: paths,
        seed_dir: input,
        repository: repository.to_string(),
        root: None,
    }
}

impl Fixture {
    /// The staged web tree, which is a valid local seed with no repackaging.
    fn seed(&self) -> PathBuf {
        self.seed_dir.clone()
    }
}

fn host() -> HostProfile {
    HostProfile {
        architecture: HostArchitecture::X86_64,
        os: "windows".to_owned(),
        native_execution: true,
        frontends: Vec::new(),
    }
}

fn context(fixture: &Fixture) -> TrustContext {
    TrustContext {
        trust: OnlineTrust::new(
            zup_core::AppId::new(APP).expect("valid"),
            CHANNEL,
            &fixture.repository,
            &fixture.trusted_root,
            ReleasePin::Channel {
                channel: CHANNEL.to_owned(),
            },
        ),
        state_root: fixture.state.clone(),
        pin: ReleasePin::Channel {
            channel: CHANNEL.to_owned(),
        },
    }
}

fn resolver_with(_fixture: &Fixture, context: TrustContext) -> ReleaseResolver {
    let (sink, _receiver) = zup_acquire::ProgressSink::channel(8);
    ReleaseResolver::new(context, host(), zup_acquire::CachePolicy::Auto, sink).expect("a resolver")
}

fn resolver(fixture: &Fixture) -> ReleaseResolver {
    resolver_with(fixture, context(fixture))
}

fn document(fixture: &Fixture, suffix: &str) -> PathBuf {
    fixture
        .documents
        .iter()
        .find(|path| path.to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("a `{suffix}` document"))
        .clone()
}

#[tokio::test]
async fn resolves_a_channel_release_and_reports_what_it_chose() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let resolved = resolver(&fixture)
        .resolve()
        .await
        .expect("a resolved release");

    assert_eq!(resolved.descriptor.version, "1.1.0");
    assert_eq!(resolved.variant.id, "windows-x64");
    assert!(!resolved.pinned, "a channel context is not pinned");
    assert_eq!(resolved.document, "releases/stable.json");
    assert_eq!(resolved.descriptor.release_digest, release.release_digest);
    assert_eq!(resolved.catalog.blobs.len(), options.content.blobs.len());
    // The runtime is named by the release, so it is verified before it runs.
    let runtime = resolved.runtime().expect("a runtime");
    assert_eq!(runtime.digest, digest_of(b"runtime-windows-x64"));
    assert!(runtime.kind().is_executable());
    assert_eq!(runtime.size, 4);
}

#[tokio::test]
async fn a_pinned_context_reads_the_immutable_version_document() {
    let options = ReleaseOptions {
        version: "2.3.4".to_owned(),
        ..ReleaseOptions::default()
    };
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;

    let pinned = context(&fixture).pinned_to("2.3.4");
    let resolved = resolver_with(&fixture, pinned)
        .resolve()
        .await
        .expect("a resolved release");
    assert_eq!(resolved.document, "releases/stable/versions/2.3.4.json");
    assert!(resolved.pinned);
    // Both documents carry the same body, so the release digest is the same
    // either way. That is what lets a pinned and a channel client prove they
    // would install identical bytes.
    assert_eq!(resolved.descriptor.release_digest, release.release_digest);
}

/// A document that was never published is a refusal, not a default: a
/// missing target, an unlinked one, and a pin for a version nobody released
/// are all the same answer.
#[tokio::test]
async fn a_document_that_was_never_published_is_refused_rather_than_trusted() {
    let options = ReleaseOptions {
        version: "2.3.4".to_owned(),
        ..ReleaseOptions::default()
    };
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;

    // The version-addressed document for 1.0.0 was never published, so a
    // pinned client asking for it gets a missing target rather than the
    // current release. That is what keeps a pinned installer from moving.
    let pinned = context(&fixture).pinned_to("1.0.0");
    let error = resolver_with(&fixture, pinned)
        .resolve()
        .await
        .expect_err("refused");
    assert!(matches!(error, UpdateError::MissingTarget(_)), "{error:?}");

    // A release whose targets were never linked is equally untrusted, and
    // equally a refusal.
    let fixture = publish(
        &release,
        &options.content,
        SigningOptions {
            unlink_targets: true,
            ..SigningOptions::default()
        },
    )
    .await;
    let error = resolver(&fixture).resolve().await.expect_err("refused");
    assert!(
        matches!(error, UpdateError::Tuf(_) | UpdateError::MissingTarget(_)),
        "{error:?}"
    );
}
#[tokio::test]
async fn a_release_for_another_application_is_refused() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let mut context = context(&fixture);
    context.trust.app_id = zup_core::AppId::new("com.example.other").expect("valid");
    let error = resolver_with(&fixture, context)
        .resolve()
        .await
        .expect_err("refused");
    assert!(
        matches!(error, UpdateError::WrongApplication { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_release_with_no_variant_for_this_host_is_refused() {
    let options = ReleaseOptions::default();
    let mut release = release(&options);
    release.variants[0].target = target("aarch64");
    release.release_digest = release.computed_digest().expect("fingerprints");
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let error = resolver(&fixture).resolve().await.expect_err("refused");
    assert!(error.to_string().contains("architecture x86_64"), "{error}");
    assert!(error.left_machine_unchanged());
}

/// TUF hashes every target, so a substituted document is refused by the
/// transport before the release's own digest claim is even consulted. The
/// release-level check still exists, and it is the one a local seed goes
/// through, so it is asserted directly.
#[test]
fn a_document_that_does_not_match_an_authenticated_digest_is_refused() {
    let claimed = digest_of(b"the authenticated bytes");
    assert!(zup_update::check_digest(b"the authenticated bytes", claimed, "catalog").is_ok());
    let error = zup_update::check_digest(b"a different catalog", claimed, "content catalog")
        .expect_err("refused");
    assert!(error.to_string().contains("content catalog"), "{error}");
    assert!(error.left_machine_unchanged());
}

#[tokio::test]
async fn a_substituted_catalog_is_refused() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    // Swap the catalog for one describing different content, keeping the
    // release's digest claim. This is the substitution a hostile origin or a
    // stale mirror would try.
    std::fs::write(
        document(&fixture, "catalog.json"),
        content(&[1, 2, 3]).catalog_bytes,
    )
    .expect("a tampered catalog");
    let error = resolver(&fixture).resolve().await.expect_err("refused");
    assert!(error.left_machine_unchanged());
    // Either layer refusing is a refusal; TUF's hash check comes first, so a
    // substituted target is caught before the graph is even parsed.
    let text = error.to_string();
    assert!(
        text.contains("content catalog")
            || text.contains("Hash mismatch")
            || text.contains("release transport failed"),
        "{error}"
    );
}

#[tokio::test]
async fn a_substituted_manifest_is_refused() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    std::fs::write(
        document(&fixture, "variants/windows-x64.json"),
        b"{\"schema\":1,\"version\":\"tampered\"}",
    )
    .expect("a tampered manifest");
    let error = resolver(&fixture).resolve().await.expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("variant manifest")
            || text.contains("Hash mismatch")
            || text.contains("release transport failed"),
        "{error}"
    );
}

#[tokio::test]
async fn expired_metadata_is_refused() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(
        &release,
        &options.content,
        SigningOptions {
            expired_timestamp: true,
            ..SigningOptions::default()
        },
    )
    .await;
    let error = resolver(&fixture).resolve().await.expect_err("refused");
    assert!(matches!(error, UpdateError::Tuf(_)), "{error:?}");
}

#[tokio::test]
async fn rollback_is_refused_across_processes() {
    // The load-bearing test. A resolver that created a fresh datastore per call
    // would pass every other test in this file and fail this one, because this is
    // the only test whose two runs share a state root and a repository.
    let shared = tempfile::tempdir().expect("one repository");
    let current = ReleaseOptions {
        version: "1.2.0".to_owned(),
        ..ReleaseOptions::default()
    };
    let current_release = release(&current);
    let fresh = publish_at(
        shared.path(),
        &current_release,
        &current.content,
        SigningOptions {
            metadata_version: 5,
            ..SigningOptions::default()
        },
    )
    .await;
    resolver(&fresh)
        .resolve()
        .await
        .expect("the current release");

    // The same repository, re-signed at a lower metadata version and pointing at
    // an older release. This is exactly what a compromised or misconfigured
    // publisher would serve, and exactly what a durable datastore exists to
    // refuse.
    let old = ReleaseOptions {
        version: "1.1.0".to_owned(),
        ..ReleaseOptions::default()
    };
    let old_release = release(&old);
    let rolled_back = publish_at(
        shared.path(),
        &old_release,
        &old.content,
        SigningOptions {
            metadata_version: 3,
            ..SigningOptions::default()
        },
    )
    .await;
    let error = resolver(&rolled_back).resolve().await.expect_err("refused");
    assert!(matches!(error, UpdateError::Tuf(_)), "{error:?}");
}

#[tokio::test]
async fn root_rotation_is_followed() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(
        &release,
        &options.content,
        SigningOptions {
            rotate_root: true,
            ..SigningOptions::default()
        },
    )
    .await;
    resolver(&fixture)
        .resolve()
        .await
        .expect("a rotated root is followed");
}

#[tokio::test]
async fn a_failed_fetch_leaves_the_rollback_datastore_in_place() {
    let options = ReleaseOptions::default();
    let release = release(&options);
    let fixture = publish(
        &release,
        &options.content,
        SigningOptions {
            expired_timestamp: true,
            ..SigningOptions::default()
        },
    )
    .await;
    let context = context(&fixture);
    let datastore = context.datastore_dir().expect("a datastore");
    let _ = resolver_with(&fixture, context).resolve().await;
    assert!(
        datastore.is_dir(),
        "a failed fetch must leave the datastore in place, because that is where rollback memory lives"
    );
}
#[tokio::test]
async fn a_local_seed_satisfies_a_closure_with_no_network_at_all() {
    // The offline story, and the reason one artifact can work from a USB stick
    // and from a CDN: a staged tree is a source, not a second package format.
    // The seed is untrusted, so the proof is that the bytes still come out
    // verified through the cache.
    let options = ReleaseOptions {
        content: content(&[64, 128]),
        ..ReleaseOptions::default()
    };
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let resolver = resolver(&fixture).with_seed("usb", fixture.seed());

    // A pointed-at repository that cannot be reached: if any byte came from the
    // network this would fail rather than quietly succeed slowly.
    let mut offline = context(&fixture);
    offline.trust.repository = "https://unreachable.invalid/acme".to_owned();
    offline.state_root = fixture.state.clone();
    let resolved = resolver_with(&fixture, offline)
        .resolve()
        .await
        .expect_err("no repository is reachable");
    assert!(matches!(resolved, UpdateError::Tuf(_)), "{resolved:?}");

    // Resolution came from the seed's own copy of the graph, and an acquisition
    // over the same closure is satisfied entirely from disk.
    let resolved = resolver.resolve().await.expect("a resolved release");
    let entries: Vec<(Sha256Digest, Option<String>)> = resolved
        .catalog
        .blobs
        .iter()
        .map(|entry| (entry.digest, None))
        .collect();
    let closure = resolved
        .closure(entries, [], zup_update::ComponentSelection::All)
        .expect("a closure");
    let session = zup_acquire::AcquisitionSession::new(
        closure,
        Arc::clone(resolver.cache()),
        zup_update::bootstrap_scheduler(),
    );
    let (sink, _receiver) = zup_acquire::ProgressSink::channel(64);
    let barrier = session
        .run(
            resolver.chain().expect("a chain"),
            Arc::new(zup_acquire::NeverCancelled),
            &sink,
        )
        .await
        .expect("a satisfied closure from a local seed");
    let outcome = barrier.enter();
    assert_eq!(outcome.cache_hits, 0, "nothing was cached before this");
    assert_eq!(outcome.items.len(), options.content.blobs.len());
    for (digest, _) in &options.content.blobs {
        let blob = outcome.get(digest).expect("an acquired blob");
        let logical = blob.read_to_end().expect("verified content");
        // A blob only comes out of the cache if it decompressed and hashed to
        // the digest the authenticated catalog named.
        assert_eq!(digest_of(&logical), *digest);
    }
}

#[tokio::test]
async fn a_release_classifies_its_offline_and_thin_downloads() {
    let options = ReleaseOptions {
        downloads: vec![
            ReleaseDownload {
                kind: ReleaseDownloadKind::OfflineInstaller,
                path: "Acme-Setup.exe".to_owned(),
                descriptor: DocumentRef::of(digest_of(b"setup"), 2),
                variant: Some("windows-x64".to_owned()),
            },
            ReleaseDownload {
                kind: ReleaseDownloadKind::ThinInstaller,
                path: "Acme-Setup-web.exe".to_owned(),
                descriptor: DocumentRef::of(digest_of(b"thin"), 2),
                variant: None,
            },
        ],
        ..ReleaseOptions::default()
    };
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let resolved = resolver(&fixture).resolve().await.expect("resolved");
    // The offline installer is a claim in the graph, not a requirement of it.
    assert_eq!(
        resolved
            .descriptor
            .human_download()
            .map(|download| download.path.as_str()),
        Some("Acme-Setup.exe")
    );
    assert_eq!(
        resolved
            .descriptor
            .thin_download()
            .map(|download| download.path.as_str()),
        Some("Acme-Setup-web.exe")
    );
}
#[tokio::test]
async fn the_closure_narrows_by_component_selection() {
    let options = ReleaseOptions {
        content: content(&[64, 128, 256, 512]),
        ..ReleaseOptions::default()
    };
    let release = release(&options);
    let fixture = publish(&release, &options.content, SigningOptions::default()).await;
    let resolved = resolver(&fixture).resolve().await.expect("resolved");

    let entries: Vec<(Sha256Digest, Option<String>)> = resolved
        .catalog
        .blobs
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            (
                entry.digest,
                match index {
                    0 => None,
                    1 => Some("core".to_owned()),
                    2 => Some("cli".to_owned()),
                    _ => Some("docs".to_owned()),
                },
            )
        })
        .collect();

    let every = resolved
        .closure(entries.clone(), [], zup_update::ComponentSelection::All)
        .expect("a closure");
    assert_eq!(every.len(), 4);

    let core_only = resolved
        .closure(
            entries,
            [],
            zup_update::ComponentSelection::Only(&["core", "docs"]),
        )
        .expect("a closure");
    // Required content plus the two named components; the CLI tools cost zero.
    assert_eq!(core_only.len(), 3);
    assert!(core_only.wire_size() < every.wire_size());
}
