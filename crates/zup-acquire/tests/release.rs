//! The release graph and the immutable web layout.
//!
//! A release descriptor is the authenticated root of everything an online
//! install does, so these tests are about what it is allowed to claim and what a
//! reader is allowed to believe.

mod common;

use common::*;
use rstest::rstest;
use zup_acquire::{
    CatalogEntry, ContentCatalog, ContentDescriptor, ContentKind, DocumentRef, OnlineTrust,
    RELEASE_SCHEMA, RelativeContentPath, ReleaseDescriptor, ReleaseDownload, ReleaseDownloadKind,
    ReleaseIdentity, ReleasePin, ReleaseVariant, ThinScope, WebLayout, blob_path, check_channel,
    check_segment,
};
use zup_core::{AppId, TargetTriple};

/// Recompute the fingerprint after a fixture edited the body it covers.
fn reseal(mut release: ReleaseDescriptor) -> ReleaseDescriptor {
    release.release_digest = release.computed_digest().expect("hashes");
    release
}

fn target(triple: &str) -> TargetTriple {
    TargetTriple::parse(triple).expect("the fixture triple parses")
}

fn variant(id: &str, triple: &str, digests: &[zup_core::Sha256Digest]) -> ReleaseVariant {
    let mut sorted = digests.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    ReleaseVariant {
        id: id.to_owned(),
        target: target(triple),
        platform: "windows-x86_64".to_owned(),
        frontend: "gui".to_owned(),
        manifest: DocumentRef::of(digest_of(b"manifest"), 8),
        runtime: Some(DocumentRef::of(digest_of(b"runtime"), 4)),
        content: sorted,
        requirements: Default::default(),
        logical_size: 1024,
    }
}

/// A variant with an empty frontend, which a client selects on.
fn frontendless(variant: ReleaseVariant) -> ReleaseVariant {
    ReleaseVariant {
        frontend: String::new(),
        ..variant
    }
}

/// The identity an installed machine persists.
fn identity() -> ReleaseIdentity {
    ReleaseIdentity {
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        release: digest_of(b"release"),
        catalog: digest_of(b"catalog"),
        variant: "windows-x64".to_owned(),
        manifest: digest_of(b"manifest"),
        runtime: Some(digest_of(b"runtime")),
        version: "1.4.0".to_owned(),
        target: target("x86_64-pc-windows-msvc"),
        channel: "stable".to_owned(),
        pinned: false,
        trust_anchor: digest_of(b"root"),
        frontend: "gui".to_owned(),
        components: vec!["core".to_owned(), "tooling".to_owned()],
    }
}

fn release(variants: Vec<ReleaseVariant>) -> ReleaseDescriptor {
    let catalog_entries: Vec<CatalogEntry> = variants
        .iter()
        .flat_map(|variant| variant.content.iter())
        .map(|digest| CatalogEntry::compressed(*digest, 10, 100))
        .collect();
    let catalog = ContentCatalog::new(catalog_entries).expect("the fixture catalog is well formed");
    let catalog_bytes = catalog.encode().expect("the catalog encodes");
    let mut release = ReleaseDescriptor {
        schema: RELEASE_SCHEMA,
        app_id: AppId::new("com.acme.desktop").expect("the fixture app id is valid"),
        channel: "stable".to_owned(),
        version: "1.4.0".to_owned(),
        release_digest: digest_of(b"placeholder"),
        catalog: DocumentRef::of(digest_of(&catalog_bytes), catalog_bytes.len() as u64),
        variants,
        downloads: Vec::new(),
    };
    release
        .variants
        .sort_by(|left, right| left.id.cmp(&right.id));
    release.release_digest = release.computed_digest().expect("the body serializes");
    release
}

#[test]
fn a_release_fingerprint_describes_the_release_and_cannot_be_forged_in_place() {
    let release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    assert_eq!(
        release.release_digest,
        release.computed_digest().expect("hashes")
    );
    release.validate().expect("the release is well formed");

    // Change one byte of a claim and the fingerprint no longer describes it.
    let mut tampered = release.clone();
    tampered.version = "9.9.9".to_owned();
    let error = tampered
        .validate()
        .expect_err("a rewritten version breaks the fingerprint");
    assert!(error.to_string().contains("fingerprint"), "{error}");
}

/// A release is refused if it describes something no machine could install, and
/// it is the *reason* a client shows that matters, so each case names its own
/// refusal.
#[rstest]
#[case::no_variants_at_all("no variants", &|r: &mut ReleaseDescriptor| {
        r.variants.clear();
    })]
#[case::content_out_of_ascending_order("ascending", &|r: &mut ReleaseDescriptor| {
        r.variants[0].content.push(digest_of(b"b"));
    })]
#[case::two_variants_for_one_target("same target", &|r: &mut ReleaseDescriptor| {
        r.variants.insert(
            0,
            variant("aaa", "x86_64-pc-windows-msvc", &[digest_of(b"b")]),
        );
    })]
#[case::a_download_naming_a_variant_it_does_not_carry("does not carry", &|r: &mut ReleaseDescriptor| {
        r.downloads.push(ReleaseDownload {
            kind: ReleaseDownloadKind::OfflineInstaller,
            path: "Acme-Setup.exe".to_owned(),
            descriptor: DocumentRef::of(digest_of(b"exe"), 1),
            variant: Some("missing".to_owned()),
        });
    })]
fn a_release_that_cannot_describe_an_installation_is_refused(
    #[case] reason: &str,
    #[case] break_it: &dyn Fn(&mut ReleaseDescriptor),
) {
    let mut release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    break_it(&mut release);
    let release = reseal(release);
    let error = release
        .validate()
        .expect_err("a release that cannot describe an installation is refused");
    assert!(error.to_string().contains(reason), "{error}");
}

#[test]
fn a_variant_a_client_cannot_select_on_is_refused() {
    // Target, platform, and frontend are what a client selects a variant by, so
    // a release that leaves one blank has described a variant nobody can choose.
    let base = variant("one", "x86_64-pc-windows-msvc", &[digest_of(b"a")]);
    let incomplete = vec![
        frontendless(base.clone()),
        ReleaseVariant {
            platform: String::new(),
            ..base.clone()
        },
    ];
    for variant in incomplete {
        let mut release = ReleaseDescriptor {
            schema: RELEASE_SCHEMA,
            app_id: AppId::new("com.acme.desktop").expect("valid"),
            channel: "stable".to_owned(),
            version: "1.4.0".to_owned(),
            release_digest: digest_of(b"x"),
            catalog: DocumentRef::of(digest_of(b"c"), 1),
            variants: vec![variant],
            downloads: Vec::new(),
        };
        release.release_digest = release.computed_digest().expect("hashes");
        assert!(
            release.validate().is_err(),
            "a blank selection field must be refused"
        );
    }
}

/// The installer republishes the release description into a TUF repository, so a
/// parse that tolerates an unknown field publishes something other than the
/// bytes it read, and a reader can no longer predict what it is verifying.
#[test]
fn a_release_document_with_an_unknown_field_is_refused() {
    let release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    let mut extended = serde_json::to_value(&release).expect("serializes");
    extended
        .as_object_mut()
        .expect("an object")
        .insert("extra".to_owned(), serde_json::json!(1));
    let error = ReleaseDescriptor::parse(&serde_json::to_vec(&extended).expect("serializes"))
        .expect_err("an unknown field is refused");
    assert!(
        matches!(error, zup_acquire::AcquireError::Json(_)),
        "{error}"
    );
}

/// Every untrusted name a release carries resolves against a root, so the rule
/// is a property of the segment rather than a character blacklist: no accepted
/// name can name a parent, a separator, a drive, or a stream.
/// Every untrusted name a release carries resolves against a root, so the rule
/// is a property of the segment rather than a character blacklist: no accepted
/// name can name a parent, a separator, a drive, or a stream. Each rule is
/// checked against a name that satisfies it and one that tries to break it.
const SEGMENT_NAMES: &[(&str, &str, bool)] = &[
    ("stable", "channel", true),
    ("lts-1", "channel", true),
    ("../etc", "channel", false),
    ("a/b", "channel", false),
    ("a\\b", "channel", false),
    ("", "channel", false),
    ("A", "channel", false),
    ("with space", "channel", false),
    ("a..b", "channel", false),
    ("windows-x64", "variant", true),
    ("linux.x64", "variant", true),
    ("../x", "variant", false),
    ("a/b", "variant", false),
    ("", "variant", false),
    (".hidden", "variant", false),
    ("a b", "variant", false),
    ("blobs/sha256/ab/cdef", "content_path", true),
    ("file.name.txt", "content_path", true),
    ("/abs", "content_path", false),
    ("\\abs", "content_path", false),
    ("C:/x", "content_path", false),
    ("a//b", "content_path", false),
    ("a/./b", "content_path", false),
    ("a/../b", "content_path", false),
    ("..", "content_path", false),
    ("a/\0/b", "content_path", false),
];

#[test]
fn an_untrusted_path_segment_is_refused_whenever_it_could_leave_a_root() {
    for (name, rule, accepted) in SEGMENT_NAMES {
        let outcome = match *rule {
            "channel" => check_channel(name).is_ok(),
            "variant" => check_segment(name).is_ok(),
            _ => RelativeContentPath::parse(name).is_ok(),
        };
        assert_eq!(outcome, *accepted, "`{name}` as a {rule}");
    }
}

/// Every path the static origin serves, in the exact form a reader and the
/// cache both address. A change here silently 404s every client, so the strings
/// are stated rather than derived.
#[test]
fn the_web_layout_places_each_document_where_a_reader_expects_it() {
    let digest = digest_of(b"one");
    let hex = digest.to_hex();
    let blob = WebLayout::blob(&digest).to_string();
    assert_eq!(blob, blob_path(&digest).to_string());
    assert_eq!(blob, format!("blobs/sha256/{}/{}", &hex[..2], &hex[2..]));
    // Two-level fan-out keeps any single directory small for a static host.
    assert_eq!(blob.matches('/').count(), 3);
    assert_ne!(
        blob,
        WebLayout::blob(&digest_of(b"two")).to_string(),
        "a different digest is a different object"
    );

    assert_eq!(
        WebLayout::release("stable").expect("a path").to_string(),
        "releases/stable.json"
    );
    assert_eq!(
        WebLayout::catalog("stable").expect("a path").to_string(),
        "releases/stable/catalog.json"
    );
    assert_eq!(
        WebLayout::variant_manifest("stable", "windows-x64")
            .expect("a path")
            .to_string(),
        "releases/stable/variants/windows-x64.json"
    );
}

/// A catalog is a set: a repeat would make the scheduler plan two transfers for
/// one object, so the order the bytes arrived in must not survive.
#[test]
fn a_catalog_is_canonical_sorted_and_deduplicated() {
    let one = digest_of(b"one");
    let two = digest_of(b"two");
    let unsorted = vec![
        CatalogEntry::compressed(two, 2, 20),
        CatalogEntry::compressed(one, 1, 10),
        CatalogEntry::compressed(two, 2, 20),
    ];
    let catalog = ContentCatalog::new(unsorted).expect("the catalog is well formed");
    assert_eq!(catalog.blobs.len(), 2, "a repeated digest is one entry");
    assert_eq!(catalog.entry(&one).map(|entry| entry.size), Some(10));
    assert_eq!(
        catalog.compressed_size(),
        3,
        "a repeat is not counted twice"
    );
}

#[test]
fn a_catalog_that_could_make_a_reader_allocate_is_refused() {
    let empty = ContentCatalog {
        schema: zup_acquire::CATALOG_SCHEMA,
        blobs: Vec::new(),
    };
    assert!(empty.validate().is_err(), "an empty catalog is refused");
    let huge = ContentCatalog {
        schema: zup_acquire::CATALOG_SCHEMA,
        blobs: vec![CatalogEntry::compressed(
            digest_of(b"a"),
            1,
            zup_acquire::MAX_PAYLOAD_BYTES + 1,
        )],
    };
    assert!(
        huge.validate().is_err(),
        "an entry beyond the size limit is refused"
    );
    let bomb = ContentCatalog {
        schema: zup_acquire::CATALOG_SCHEMA,
        blobs: vec![CatalogEntry::compressed(
            digest_of(b"a"),
            1024,
            4096 * 1024 * 1024,
        )],
    };
    assert!(
        bomb.validate().is_err(),
        "an expansion beyond the ratio limit is refused"
    );
}

/// A thin artifact carries identity, a channel, a location, a scope and a root.
/// It carries no payload, no runtime and no other architecture, so the block a
/// launcher has to download before it can resolve anything at all stays small.
#[rstest]
#[case::a_channel_pin(ReleasePin::Channel { channel: "stable".to_owned() }, false)]
#[case::a_version_pin(ReleasePin::Version { version: "1.4.0".to_owned() }, true)]
fn a_thin_bootstrapper_carries_only_what_it_needs_to_resolve_a_release(
    #[case] pin: ReleasePin,
    #[case] pinned: bool,
) {
    let trust = OnlineTrust {
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        channel: "stable".to_owned(),
        repository: "https://updates.example.com/acme".to_owned(),
        trusted_root: zup_core::base64_encode(&[b'{'; 512]),
        pin,
        mirrors: vec!["https://mirror.internal/acme".to_owned()],
        scope: ThinScope::Machine,
    };
    trust
        .validate()
        .expect("the trust configuration is well formed");
    assert_eq!(trust.scope.selected(), zup_core::SelectedScope::Machine);
    assert_eq!(trust.pin.is_pinned(), pinned);

    let bytes = serde_json::to_vec(&trust).expect("it serializes");
    // The dominant term is the base64-encoded trusted root, which is why a root
    // is a separate resource rather than something the bootstrapper re-encodes.
    assert!(
        bytes.len() < 4096,
        "the trust block is {} bytes",
        bytes.len()
    );
    let parsed: OnlineTrust = serde_json::from_slice(&bytes).expect("it parses");
    assert_eq!(parsed, trust, "the block does not survive its own encoding");

    // A pinned build addresses the immutable version-addressed release, so a
    // later publication cannot move it; a channel one addresses the channel
    // pointer, which is exactly the promise it makes and exactly as little.
    let document = trust.release_document().expect("addressable").to_string();
    assert_eq!(
        document,
        if pinned {
            "releases/stable/versions/1.4.0.json".to_owned()
        } else {
            "releases/stable.json".to_owned()
        }
    );
    assert_eq!(trust.pinned_version(), pinned.then_some("1.4.0"));
}

#[test]
fn a_trust_configuration_that_points_somewhere_unexpected_is_refused() {
    let base = OnlineTrust {
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        channel: "stable".to_owned(),
        repository: "https://updates.example.com/acme".to_owned(),
        trusted_root: zup_core::base64_encode(&[b'{'; 16]),
        pin: ReleasePin::Channel {
            channel: "stable".to_owned(),
        },
        mirrors: Vec::new(),
        scope: ThinScope::User,
    };
    assert!(base.validate().is_ok());
    for hostile in [
        OnlineTrust {
            channel: "../evil".to_owned(),
            ..base.clone()
        },
        OnlineTrust {
            repository: String::new(),
            ..base.clone()
        },
        OnlineTrust {
            trusted_root: String::new(),
            ..base.clone()
        },
        OnlineTrust {
            trusted_root: "not base64 !!".to_owned(),
            ..base.clone()
        },
        OnlineTrust {
            trusted_root: zup_core::base64_encode(&vec![
                b'{';
                (zup_acquire::OnlineTrust::MAX_ROOT_BYTES + 1)
                    as usize
            ]),
            ..base.clone()
        },
        OnlineTrust {
            mirrors: (0..9).map(|index| format!("https://m{index}")).collect(),
            ..base.clone()
        },
        OnlineTrust {
            pin: ReleasePin::Version {
                version: String::new(),
            },
            ..base.clone()
        },
    ] {
        assert!(hostile.validate().is_err(), "{hostile:?} must be refused");
    }
}

#[test]
fn an_installed_machine_records_the_graph_not_just_the_version() {
    let identity = identity();
    identity.validate().expect("the identity is well formed");
    let bytes = identity.encode().expect("the identity encodes");
    let parsed: ReleaseIdentity = serde_json::from_slice(&bytes).expect("it parses");
    assert_eq!(parsed, identity);

    // Two builds can claim the same version and install different bytes. Only
    // the digest tells them apart, which is why it is the field that is trusted.
    let other = ReleaseIdentity {
        release: digest_of(b"a different build"),
        ..identity.clone()
    };
    assert_ne!(other.release, identity.release);
    assert_eq!(other.version, identity.version);
    assert!(!other.same_release(&identity));
    assert!(identity.same_release(&identity));
    assert!(serde_json::to_vec(&other).expect("serializes") != bytes);
}

#[rstest]
#[case::a_blank_version(&|id: &mut ReleaseIdentity| id.version = String::new())]
#[case::a_variant_that_climbs(&|id: &mut ReleaseIdentity| id.variant = "../x".to_owned())]
#[case::a_component_that_climbs(&|id: &mut ReleaseIdentity| {
    id.components = vec!["../../etc/passwd".to_owned()];
})]
fn a_release_identity_that_could_not_describe_an_installation_is_refused(
    #[case] break_it: &dyn Fn(&mut ReleaseIdentity),
) {
    let mut identity = identity();
    identity.validate().expect("the identity is well formed");
    break_it(&mut identity);
    assert!(identity.validate().is_err(), "{identity:?} must be refused");
}

/// The runtime is what control is eventually handed to, so it is named as
/// content the engine fetches, verified, and schedules first - not treated as
/// something already present.
#[test]
fn a_release_names_a_runtime_as_content_so_it_is_verified_before_it_runs() {
    let release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    let runtime = release
        .runtime_descriptor(&release.variants[0])
        .expect("an online release names the runtime the machine has to fetch");
    assert_eq!(
        runtime.priority,
        zup_acquire::ContentPriority::Critical,
        "a runtime is what control is handed to, so it is scheduled first"
    );
    assert!(
        runtime.kind().is_executable(),
        "only a runtime is executable, so only a runtime gets run"
    );
}

#[test]
fn a_release_prefers_the_offline_installer_when_a_human_is_clicks() {
    let mut release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    release.downloads = vec![
        ReleaseDownload {
            kind: ReleaseDownloadKind::ThinInstaller,
            path: "Acme-Setup-online.exe".to_owned(),
            descriptor: DocumentRef::of(digest_of(b"thin"), 2),
            variant: None,
        },
        ReleaseDownload {
            kind: ReleaseDownloadKind::OfflineInstaller,
            path: "Acme-Setup.exe".to_owned(),
            descriptor: DocumentRef::of(digest_of(b"offline"), 2),
            variant: Some("windows-x64".to_owned()),
        },
    ];
    let release = reseal(release);
    release.validate().expect("well formed");
    assert_eq!(
        release.human_download().map(|d| d.path.as_str()),
        Some("Acme-Setup.exe"),
        "a disconnected machine still gets one file"
    );
    assert_eq!(
        release.thin_download().map(|d| d.path.as_str()),
        Some("Acme-Setup-online.exe")
    );
}

#[test]
fn a_document_reference_is_bounded_by_the_kind_it_describes() {
    assert!(
        DocumentRef::of(digest_of(b"a"), 0)
            .validate(ContentKind::Metadata)
            .is_err()
    );
    assert!(
        DocumentRef::of(digest_of(b"a"), zup_acquire::MAX_METADATA_BYTES + 1)
            .validate(ContentKind::Metadata)
            .is_err()
    );
    assert!(
        DocumentRef::of(digest_of(b"a"), 8)
            .validate(ContentKind::Metadata)
            .is_ok()
    );
}

#[test]
fn a_descriptor_kind_decides_how_deeply_a_cached_blob_is_rechecked() {
    use zup_acquire::Verify;
    // Payload is the only kind a length check is allowed to stand in for; every
    // kind that can be executed or parsed is re-read and re-hashed.
    assert_eq!(Verify::for_kind(ContentKind::Payload), Verify::WireLength);
    for kind in [
        ContentKind::Runtime,
        ContentKind::Metadata,
        ContentKind::Catalog,
    ] {
        assert_eq!(
            Verify::for_kind(kind),
            Verify::Full,
            "{kind} is fully rechecked"
        );
    }
}

/// One digest is one item, so it is downloaded and accounted exactly once. Which
/// group the report files it under is a presentation choice, not a second
/// transfer.
#[test]
fn a_closure_summarizes_itself_by_group() {
    let shared = payload(72, 4 * 1024);
    let descriptor = ContentDescriptor::compressed(
        ContentKind::Payload,
        digest_of(&shared),
        512,
        shared.len() as u64,
    );
    let plan = zup_acquire::AcquisitionPlan::build(vec![
        zup_acquire::AcquisitionItem::new(
            descriptor,
            zup_acquire::ContentReason::Prerequisite {
                id: "vcredist".to_owned(),
            },
        ),
        zup_acquire::AcquisitionItem::new(
            descriptor,
            zup_acquire::ContentReason::File {
                component: Some("core".to_owned()),
            },
        ),
    ])
    .expect("the closure is well formed");
    assert_eq!(
        plan.len(),
        1,
        "one digest wanted for two reasons is one item"
    );
    assert!(
        plan.summary().contains("1 blobs"),
        "the summary counts it once"
    );
    let by_group = plan.wire_size_by_group();
    assert_eq!(by_group.len(), 1, "one digest is one group entry");
    assert_eq!(by_group.values().next().copied(), Some(512));
    assert_eq!(plan.install_size(), shared.len() as u64);
}

/// A digest the catalog does not describe is a refusal, not a zero-size item:
/// planning it anyway would download nothing and report success.
#[test]
fn a_closure_refuses_a_digest_its_catalog_does_not_describe() {
    let one = payload(70, 8 * 1024);
    let (catalog, _) = catalog(&[(&one, LEVEL)]);
    let error = zup_acquire::AcquisitionPlan::for_variant(
        &catalog,
        &[digest_of(b"unknown")],
        ContentKind::Payload,
    )
    .expect_err("an uncatalogued digest is refused");
    assert!(
        matches!(error, zup_acquire::AcquireError::Missing { .. }),
        "{error}"
    );
}

/// The one place byte counts are turned into text. Every frontend renders them
/// through this, so the unit changeover and the trailing-digit rule are stated
/// once here rather than in each caller.
#[test]
fn byte_counts_render_the_way_every_frontend_renders_them() {
    for (bytes, rendered) in [
        (0, "0 B"),
        (1024, "1.00 KiB"),
        (1536, "1.50 KiB"),
        (526 * 1024 * 1024, "526 MiB"),
    ] {
        assert_eq!(zup_acquire::format_bytes(bytes), rendered, "{bytes} bytes");
    }
}
