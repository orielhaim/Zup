use zup_publish_github::GithubHost;

#[test]
fn a_download_url_names_the_release_and_the_asset() {
    let host = GithubHost::dotcom();
    assert_eq!(
        host.download_url("acme/acme", "v1.4.0", "Acme-Windows-Setup.exe"),
        "https://github.com/acme/acme/releases/download/v1.4.0/Acme-Windows-Setup.exe"
    );
    assert_eq!(
        host.latest_download_url("acme/acme", "Acme-Windows-Setup.exe"),
        "https://github.com/acme/acme/releases/latest/download/Acme-Windows-Setup.exe"
    );
    assert_eq!(
        host.download_url("acme/acme", "v1.4.0 rc1", "Acme-Windows-Setup.exe"),
        "https://github.com/acme/acme/releases/download/v1.4.0%20rc1/Acme-Windows-Setup.exe"
    );
}
