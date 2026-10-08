use rstest::rstest;
use zup_core::{Sha256Digest, hash_bytes};
use zup_publish::{
    AssetAction, ConflictReason, ProductClass, ProductRole, ReleaseProduct, RemoteAsset,
    asset_name, check_asset_name, classify, document_name, document_path, package_name,
    safe_segment, shard_index, shard_name,
};

fn product(name: &str, seed: &[u8], size: u64) -> ReleaseProduct {
    ReleaseProduct::new(
        name,
        ProductRole::Install,
        ProductClass::Transport,
        hash_bytes(seed),
        size,
    )
}

fn digest(seed: &[u8]) -> Sha256Digest {
    hash_bytes(seed)
}

#[test]
fn missing_remote_means_upload() {
    let action = classify(&product("app.zup", b"a", 10), None);
    assert_eq!(action, AssetAction::Upload);
    assert!(action.uploads());
    assert!(!action.is_conflict());
}

#[test]
fn matching_digest_and_size_is_present() {
    let product = product("app.zup", b"a", 10);
    let remote = RemoteAsset::uploaded("app.zup", 10, product.digest);
    let action = classify(&product, Some(&remote));
    assert_eq!(action, AssetAction::Present);
    assert!(!action.uploads());
    assert!(action.conflict().is_none());
}

#[rstest]
#[case(b"a", b"b", 10, 10, ConflictReason::DifferentBytes)]
#[case(b"a", b"a", 10, 11, ConflictReason::DifferentBytes)]
fn digest_or_size_mismatch_is_a_conflict(
    #[case] want: &[u8],
    #[case] have: &[u8],
    #[case] want_size: u64,
    #[case] have_size: u64,
    #[case] reason: ConflictReason,
) {
    let product = product("app.zup", want, want_size);
    let remote = RemoteAsset::uploaded("app.zup", have_size, digest(have));
    let action = classify(&product, Some(&remote));
    let conflict = action.conflict().expect("a mismatch is a conflict");
    assert_eq!(conflict.reason, reason);
    assert!(action.is_conflict());
    assert!(!action.uploads());
}

#[test]
fn starter_remote_is_a_removable_replace() {
    let product = product("app.zup", b"a", 10);
    let remote = RemoteAsset::starter("app.zup");
    let action = classify(&product, Some(&remote));
    let conflict = action.conflict().expect("a starter is replaced");
    assert_eq!(conflict.reason, ConflictReason::FailedUpload);
    assert!(conflict.reason.is_removable());
    assert!(action.uploads());
}

#[test]
fn remote_without_digest_is_unverifiable() {
    let product = product("app.zup", b"a", 10);
    let remote = RemoteAsset::opaque("app.zup", Some(10));
    let action = classify(&product, Some(&remote));
    let conflict = action.conflict().expect("no digest means no trust");
    assert_eq!(conflict.reason, ConflictReason::Unverifiable);
    assert!(!conflict.reason.is_removable());
}

#[rstest]
#[case("zup-release-stable.json", "releases/stable.json")]
#[case("zup-release-stable-1.4.2.json", "releases/stable/versions/1.4.2.json")]
#[case("zup-catalog-stable.json", "releases/stable/catalog.json")]
#[case("zup-tuf-root.json", "metadata/root.json")]
fn document_names_resolve_to_paths(#[case] name: &str, #[case] path: &str) {
    assert_eq!(document_path(name).expect("a document").to_string(), path);
}

#[rstest]
#[case("nope.json")]
#[case("zup-blobs.json")]
#[case("")]
fn non_documents_are_rejected(#[case] name: &str) {
    assert!(document_path(name).is_err());
}

#[test]
fn document_names_round_trip() {
    use zup_publish::DocumentKind;
    for (kind, name) in [
        (
            DocumentKind::Release {
                channel: "stable",
                version: None,
            },
            "zup-release-stable.json",
        ),
        (
            DocumentKind::Release {
                channel: "stable",
                version: Some("1.4.2"),
            },
            "zup-release-stable-1.4.2.json",
        ),
        (
            DocumentKind::Catalog { channel: "stable" },
            "zup-catalog-stable.json",
        ),
        (
            DocumentKind::TufMetadata {
                version_role: "root",
            },
            "zup-tuf-root.json",
        ),
    ] {
        assert_eq!(document_name(&kind), name);
        assert!(document_path(name).is_ok());
    }
}

#[test]
fn package_and_shard_names_round_trip() {
    let package = package_name("Aurora Draw", "win-x64");
    assert!(package.ends_with(".zup"));
    assert!(check_asset_name(&package).is_ok());
    assert_eq!(shard_index(&shard_name(&package, 7)), Some(7));
    assert_eq!(shard_index(&package), None);
    assert_eq!(shard_index("app.zup.1"), None);
}

#[rstest]
#[case("app.zup", true)]
#[case("", false)]
#[case(".hidden", false)]
#[case("trailing.", false)]
#[case("has space", false)]
#[case("con.zup", false)]
#[case("aux", false)]
fn asset_name_policy(#[case] name: &str, #[case] valid: bool) {
    assert_eq!(check_asset_name(name).is_ok(), valid);
}

#[test]
fn content_paths_are_not_assets() {
    let path = document_path("zup-release-stable.json").expect("a document");
    assert_eq!(
        asset_name(&path).expect("an asset"),
        "zup-release-stable.json"
    );
}

#[test]
fn safe_segment_falls_back_to_release() {
    assert_eq!(safe_segment("...", "---", "-"), "release");
    assert_eq!(safe_segment("Aurora", "win x64", "-"), "Aurora-win-x64");
}
