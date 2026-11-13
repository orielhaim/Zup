use futures_util::StreamExt;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zup_acquire_http::{HttpClient, HttpClientConfig, HttpError, Origin, TimeoutPolicy};

fn client() -> HttpClient {
    HttpClient::new(&HttpClientConfig {
        use_platform_proxy: false,
        total: 2,
        per_origin: 1,
        timeouts: TimeoutPolicy {
            connect: Duration::from_secs(10),
            headers: Duration::from_secs(1),
            idle: Duration::from_secs(2),
            per_blob: Duration::from_secs(30),
        },
    })
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn headers_have_their_own_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/blob", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let origin = Origin::site(&url).unwrap();
    let client = client();
    let request = tokio::spawn(async move { client.get(&origin, &url, 0).await });
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut buf = [0; 1024];
    assert!(socket.read(&mut buf).await.unwrap() > 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        request.await.unwrap(),
        Err(HttpError::Timeout { phase: "headers" })
    ));
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_wakes_a_stalled_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/blob", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let origin = Origin::site(&url).unwrap();
    let client = client();
    let request = tokio::spawn(async move { client.get(&origin, &url, 0).await });
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut buf = [0; 1024];
    assert!(socket.read(&mut buf).await.unwrap() > 0);
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
        .await
        .unwrap();
    let mut stream = request.await.unwrap().unwrap().bytes_stream();
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"a");
    let body = tokio::spawn(async move { stream.next().await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(matches!(
        body.await.unwrap(),
        Some(Err(HttpError::Timeout { phase: "idle" }))
    ));
}

#[tokio::test]
async fn origin_permit_survives_headers_and_is_released_with_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: url::Url = format!("http://{}/first", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let origin = Origin::site(&url).unwrap();
    let client = client();
    let first = tokio::spawn({
        let client = client.clone();
        let origin = origin.clone();
        let url = url.clone();
        async move { client.get(&origin, &url, 0).await }
    });
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut buf = [0; 1024];
    assert!(socket.read(&mut buf).await.unwrap() > 0);
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
        .await
        .unwrap();
    let response = first.await.unwrap().unwrap();
    let second =
        tokio::spawn(async move { client.get(&origin, &url.join("second").unwrap(), 0).await });
    tokio::task::yield_now().await;
    assert!(!second.is_finished());
    drop(response);
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(socket.read(&mut buf).await.unwrap() > 0);
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    assert!(second.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn retry_after_cannot_extend_the_logical_blob_deadline() {
    use zup_acquire::{
        AcquireRequest, ArtifactSource, CachePolicy, ContentCache, ContentDescriptor, ContentKind,
        NeverCancelled, SourceError,
    };
    use zup_acquire_http::{HttpSource, OriginSet};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let source = HttpSource::new(
        "loopback",
        client(),
        OriginSet::from_urls(&url, Vec::<&str>::new()).unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let cache = ContentCache::open(root.path(), CachePolicy::Keep).unwrap();
    let descriptor = ContentDescriptor::stored(
        ContentKind::Metadata,
        zup_core::Sha256Digest::from_bytes([1; 32]),
        10,
    );
    let acquire = tokio::spawn(async move {
        source
            .acquire(AcquireRequest::new(&descriptor, &cache, &NeverCancelled))
            .await
    });
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut buf = [0; 1024];
    assert!(socket.read(&mut buf).await.unwrap() > 0);
    socket
        .write_all(b"HTTP/1.1 503 Unavailable\r\nRetry-After: 300\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(matches!(
        acquire.await.unwrap(),
        Err(SourceError::Timeout { phase: "blob", .. })
    ));
}
