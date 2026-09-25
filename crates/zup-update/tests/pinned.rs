use tempfile::TempDir;
use zup_core::Sha256Digest;

#[tokio::test]
async fn pinned_download_rejects_non_https_urls_before_io() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("artifact.exe");
    let error = zup_update::download_pinned(
        "http://example.test/artifact.exe",
        Sha256Digest::from_bytes([0; 32]),
        None,
        &destination,
        1024,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, zup_update::UpdateError::Descriptor(_)));
    assert!(!destination.exists());
}

#[tokio::test]
async fn pinned_download_rejects_credentials_and_fragments() {
    let root = TempDir::new().unwrap();
    for url in [
        "https://user:password@example.test/artifact.exe",
        "https://example.test/artifact.exe#fragment",
    ] {
        let error = zup_update::download_pinned(
            url,
            Sha256Digest::from_bytes([0; 32]),
            None,
            &root.path().join("artifact.exe"),
            1024,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, zup_update::UpdateError::Descriptor(_)));
    }
}
