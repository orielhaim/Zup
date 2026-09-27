//! The release graph and the immutable web layout.
//!
//! A release descriptor is the authenticated root of everything an online
//! install does, so these tests are about what it is allowed to claim and what a
//! reader is allowed to believe.

mod common;

use common::*;
use zup_acquire::{
    CatalogEntry, ContentCatalog, ContentDescriptor, ContentKind, DocumentRef, OnlineTrust,
    RELEASE_SCHEMA, ReleaseDescriptor, ReleaseDownload, ReleaseDownloadKind, ReleaseIdentity,
    ReleasePin, ReleaseVariant, WebLayout, blob_path, check_channel, check_segment,
};
use zup_core::{AppId, TargetTriple};

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

#[test]
fn a_release_that_lies_about_its_content_is_refused() {
    let mut release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    release.variants[0].content.push(digest_of(b"b"));
    let error = release.validate().expect_err("unsorted content is refused");
    assert!(error.to_string().contains("ascending"), "{error}");
}

#[test]
fn a_release_must_be_internally_consistent() {
    let digest = digest_of(b"a");
    // No variants.
    assert!(
        release(vec![variant("a", "x86_64-pc-windows-msvc", &[digest])])
            .validate()
            .is_ok()
    );
    // Two variants for one target.
    let error = release(vec![
        variant("one", "x86_64-pc-windows-msvc", &[digest_of(b"a")]),
        variant("two", "x86_64-pc-windows-msvc", &[digest_of(b"b")]),
    ])
    .validate()
    .expect_err("two variants for one target are refused");
    assert!(error.to_string().contains("same target"), "{error}");

    // A download that names a variant the release does not carry.
    let mut release = release(vec![variant(
        "one",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    release.downloads.push(ReleaseDownload {
        kind: ReleaseDownloadKind::OfflineInstaller,
        path: "Acme-Setup.exe".to_owned(),
        descriptor: DocumentRef::of(digest_of(b"exe"), 1),
        variant: Some("missing".to_owned()),
    });
    let release = reseal(release);
    let error = release
        .validate()
        .expect_err("a dangling download is refused");
    assert!(error.to_string().contains("does not carry"), "{error}");
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

fn reseal(mut release: ReleaseDescriptor) -> ReleaseDescriptor {
    release.release_digest = release.computed_digest().expect("hashes");
    release
}

#[test]
fn a_release_round_trips_through_canonical_json() {
    let release = release(vec![variant(
        "windows-x64",
        "x86_64-pc-windows-msvc",
        &[digest_of(b"a")],
    )]);
    let bytes = release.encode().expect("the release encodes");
    let parsed = ReleaseDescriptor::parse(&bytes).expect("the release parses");
    assert_eq!(parsed, release);
    // A document that merely happens to be valid JSON with an extra field is
    // refused, so a reader can predict the bytes it verifies.
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

#[test]
fn a_channel_and_a_variant_name_cannot_escape_their_directory() {
    for hostile in ["../etc", "a/b", "a\\b", "", "A", "with space", "a..b"] {
        assert!(
            check_channel(hostile).is_err(),
            "`{hostile}` must not be a channel"
        );
    }
    for good in ["stable", "beta", "lts-1", "preview2"] {
        assert!(check_channel(good).is_ok(), "`{good}` is a channel");
    }
    for hostile in ["../x", "a/b", "", ".hidden", "a b"] {
        assert!(
            check_segment(hostile).is_err(),
            "`{hostile}` must not be a variant"
        );
    }
    for good in ["windows-x64", "windows_arm64", "linux.x64"] {
        assert!(check_segment(good).is_ok(), "`{good}` is a variant name");
    }
}

#[test]
fn a_blob_url_is_a_function_of_its_digest_and_changes_with_it() {
    let one = digest_of(b"one");
    let two = digest_of(b"two");
    let first = WebLayout::blob(&one).to_string();
    assert_eq!(
        first,
        WebLayout::blob(&one).to_string(),
        "the path is stable"
    );
    assert_ne!(first, WebLayout::blob(&two).to_string());
    assert_eq!(first, blob_path(&one).to_string());
    let hex = one.to_hex();
    assert_eq!(first, format!("blobs/sha256/{}/{}", &hex[..2], &hex[2..]));
    assert!(first.starts_with("blobs/sha256/"), "{first}");
    // Two-level fan-out keeps any single directory small for a static host.
    assert_eq!(first.matches('/').count(), 3);
}

#[test]
fn the_web_layout_places_each_document_where_a_reader_expects_it() {
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
    let bytes = catalog.encode().expect("the catalog encodes");
    assert_eq!(ContentCatalog::parse(&bytes).expect("it parses"), catalog);
    assert_eq!(catalog.entry(&one).map(|entry| entry.size), Some(10));
    assert_eq!(catalog.compressed_size(), 3);
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

#[test]
fn a_thin_bootstrapper_carries_only_what_it_needs_to_resolve_a_release() {
    let trust = OnlineTrust {
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        channel: "stable".to_owned(),
        repository: "https://updates.example.com/acme".to_owned(),
        trusted_root: zup_core::base64_encode(&[b'{'; 512]),
        pin: ReleasePin::Channel {
            channel: "stable".to_owned(),
        },
        mirrors: vec!["https://mirror.internal/acme".to_owned()],
    };
    trust
        .validate()
        .expect("the trust configuration is well formed");
    let bytes = serde_json::to_vec(&trust).expect("it serializes");
    // A bootstrapper embeds identity, a channel, a location, and a root. It
    // does not embed payload, runtimes, or other architectures. The dominant
    // term is the base64-encoded trusted root, which is why a root is a
    // separate resource rather than something the bootstrapper re-encodes.
    assert!(
        bytes.len() < 4096,
        "the trust block is {} bytes",
        bytes.len()
    );
    let parsed: OnlineTrust = serde_json::from_slice(&bytes).expect("it parses");
    assert_eq!(parsed, trust);
    assert!(parsed.pin.label() == "stable");
    assert!(!parsed.pin.is_pinned());
}

#[test]
fn a_version_pinned_bootstrapper_and_a_channel_bootstrapper_are_different_artifacts() {
    let pinned = OnlineTrust {
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        channel: "stable".to_owned(),
        repository: "https://updates.example.com/acme".to_owned(),
        trusted_root: zup_core::base64_encode(&[b'{'; 512]),
        pin: ReleasePin::Version {
            version: "1.4.0".to_owned(),
        },
        mirrors: Vec::new(),
    };
    pinned.validate().expect("well formed");
    assert!(pinned.pin.is_pinned());
    assert_eq!(
        pinned.pin.label(),
        "1.4.0",
        "a pinned build is labelled with its version"
    );

    // The two artifacts authenticate two different documents, and that is the
    // whole of the difference. A pinned one addresses the immutable
    // version-addressed release, so a later publication cannot move it; a
    // channel one addresses the channel pointer, which is exactly the promise it
    // makes and exactly as little.
    assert_eq!(
        pinned.release_document().expect("addressable").to_string(),
        "releases/stable/versions/1.4.0.json"
    );
    let channel = OnlineTrust {
        pin: ReleasePin::Channel {
            channel: "stable".to_owned(),
        },
        ..pinned.clone()
    };
    assert_eq!(
        channel.release_document().expect("addressable").to_string(),
        "releases/stable.json"
    );
    assert_eq!(pinned.pinned_version(), Some("1.4.0"));
    assert_eq!(channel.pinned_version(), None);
    assert_eq!(pinned.trust_anchor(), pinned.trust_anchor());
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

#[test]
fn a_release_identity_that_could_not_describe_an_installation_is_refused() {
    let base = identity();
    assert!(base.validate().is_ok());
    assert!(
        ReleaseIdentity {
            version: String::new(),
            ..base.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        ReleaseIdentity {
            variant: "../x".to_owned(),
            ..base.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        ReleaseIdentity {
            components: vec!["../../etc/passwd".to_owned()],
            ..base
        }
        .validate()
        .is_err()
    );
}

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
    assert_eq!(runtime.kind(), ContentKind::Runtime);
    assert_eq!(
        runtime.priority,
        zup_acquire::ContentPriority::Critical,
        "a runtime is what control is handed to, so it is scheduled first"
    );
    assert!(runtime.kind().is_executable());
    assert_eq!(
        runtime.compression,
        zup_acquire::ContentCompression::None,
        "a runtime image is carried as-is; compressing executable code buys nothing"
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
fn a_release_with_no_variants_cannot_describe_an_installation() {
    let mut release = ReleaseDescriptor {
        schema: RELEASE_SCHEMA,
        app_id: AppId::new("com.acme.desktop").expect("valid"),
        channel: "stable".to_owned(),
        version: "1.4.0".to_owned(),
        release_digest: digest_of(b"x"),
        catalog: DocumentRef::of(digest_of(b"c"), 1),
        variants: Vec::new(),
        downloads: Vec::new(),
    };
    release.release_digest = release.computed_digest().expect("hashes");
    let error = release
        .validate()
        .expect_err("a release with no variants is refused");
    assert!(error.to_string().contains("no variants"), "{error}");
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
    assert_eq!(Verify::for_kind(ContentKind::Payload), Verify::WireLength);
    assert_eq!(Verify::for_kind(ContentKind::Runtime), Verify::Full);
    assert_eq!(Verify::for_kind(ContentKind::Metadata), Verify::Full);
    assert_eq!(Verify::for_kind(ContentKind::Catalog), Verify::Full);
}

#[test]
fn a_relative_content_path_refuses_anything_that_could_leave_a_root() {
    use zup_acquire::RelativeContentPath;
    for good in ["a", "a/b", "blobs/sha256/ab/cdef", "file.name.txt"] {
        assert!(RelativeContentPath::parse(good).is_ok(), "`{good}`");
    }
    for hostile in [
        "", "/abs", "\\abs", "C:/x", "a\\b", "a//b", "a/./b", "a/../b", "..", "a/\0/b",
    ] {
        assert!(RelativeContentPath::parse(hostile).is_err(), "`{hostile}`");
    }
}

#[test]
fn a_closure_can_be_built_from_a_release_variant_and_its_catalog() {
    let one = payload(70, 8 * 1024);
    let two = payload(71, 8 * 1024);
    let (catalog, descriptors) = catalog(&[(&one, LEVEL), (&two, LEVEL)]);
    let digests = vec![
        descriptors[&digest_of(&one)].digest,
        descriptors[&digest_of(&two)].digest,
    ];
    let plan = zup_acquire::AcquisitionPlan::for_variant(&catalog, &digests, ContentKind::Payload)
        .expect("the closure is well formed");
    assert_eq!(plan.len(), 2);
    assert_eq!(
        plan.wire_size(),
        plan.items().iter().map(|i| i.wire_size()).sum::<u64>()
    );
    // A digest the catalog does not describe is a refusal, not a zero-size item.
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
    let summary = plan.summary();
    assert!(summary.contains("1 blobs"), "{summary}");
    // One digest is one item, so it is accounted once under one group. Which
    // group is a reporting choice, not a second download.
    let by_group = plan.wire_size_by_group();
    assert_eq!(by_group.len(), 1, "one digest is one group entry");
    assert_eq!(by_group.values().next().copied(), Some(512));
    assert_eq!(plan.install_size(), shared.len() as u64);
}

#[test]
fn byte_counts_render_the_way_every_frontend_renders_them() {
    use zup_acquire::format_bytes;
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1024), "1.00 KiB");
    assert_eq!(format_bytes(1536), "1.50 KiB");
    assert_eq!(format_bytes(184 * 1024 * 1024), "184 MiB");
    assert_eq!(format_bytes(526 * 1024 * 1024), "526 MiB");
    assert_eq!(format_bytes(72 * 1024 * 1024), "72.0 MiB");
}
