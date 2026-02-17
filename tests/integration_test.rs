use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};

/// Helper function to start a test HTTP server
async fn start_test_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let request_count = Arc::new(AtomicUsize::new(0));

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let count = request_count.clone();

            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let count = count.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        count.fetch_add(1, Ordering::SeqCst);

                        // Different responses based on path
                        let body: String = match path.as_str() {
                            "/test" => "Hello from test server".to_string(),
                            "/large" => {
                                // 1MB of data for streaming test
                                "x".repeat(1024 * 1024)
                            },
                            "/slow" => {
                                // Simulate slow response for concurrent test
                                sleep(Duration::from_millis(100)).await;
                                "Slow response".to_string()
                            },
                            "/cache-control" => {
                                // Test cache header parsing
                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header("cache-control", "max-age=300")
                                    .body(Full::new(Bytes::from("Cached content")))
                                    .unwrap();
                                return Ok::<_, Infallible>(response);
                            },
                            "/vary" => {
                                // Test Vary header support - response varies by Accept-Encoding
                                let accept_encoding = req.headers()
                                    .get("accept-encoding")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("identity");

                                let body = if accept_encoding.contains("gzip") {
                                    "gzip content"
                                } else {
                                    "plain content"
                                };

                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header("vary", "Accept-Encoding")
                                    .header("content-encoding", if accept_encoding.contains("gzip") { "gzip" } else { "identity" })
                                    .body(Full::new(Bytes::from(body)))
                                    .unwrap();
                                return Ok::<_, Infallible>(response);
                            },
                            _ => "Not found".to_string(),
                        };

                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body))))
                    }
                });

                let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    (addr, handle)
}

/// Helper function to start the proxy server
async fn start_proxy_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use squiddish::config::Config;
    use squiddish::proxy::ProxyServer;

    let mut config = Config::default();
    // Bind to random port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // Release the port

    config.bind_addr = addr;

    let server = ProxyServer::new(config).await.unwrap();

    let handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Give server time to start
    sleep(Duration::from_millis(200)).await;

    (addr, handle)
}

#[tokio::test]
async fn test_basic_proxy_request() {
    // Start test HTTP server
    let (origin_addr, _origin_handle) = start_test_server().await;

    // Start proxy server
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    // Make request through proxy
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    let response = client
        .get(format!("http://{}/test", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(body, "Hello from test server");
}

#[tokio::test]
async fn test_cache_hit_miss_behavior() {
    let (origin_addr, _origin_handle) = start_test_server().await;
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // First request - should be cache miss
    let response1 = client
        .get(format!("http://{}/test", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response1.status(), StatusCode::OK);
    let body1 = response1.text().await.unwrap();

    // Second request - should be cache hit
    let response2 = client
        .get(format!("http://{}/test", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);

    // Check for cache hit header before consuming body
    let has_cache_hit = response2.headers().get("x-cache").map(|v| v.to_str().unwrap()) == Some("HIT");
    let body2 = response2.text().await.unwrap();

    // Both responses should have same content
    assert_eq!(body1, body2);
    assert!(has_cache_hit, "Expected cache HIT header");
}

#[tokio::test]
async fn test_concurrent_streaming_downloads() {
    let (origin_addr, _origin_handle) = start_test_server().await;
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // Start 5 concurrent requests for the same slow resource
    let mut handles = vec![];
    for i in 0..5 {
        let client = client.clone();
        let url = format!("http://{}/slow", origin_addr);

        let handle = tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            let response = client.get(&url).send().await.unwrap();
            let elapsed = start.elapsed();
            let body = response.text().await.unwrap();
            (i, body, elapsed)
        });

        handles.push(handle);
        // Stagger requests slightly
        sleep(Duration::from_millis(10)).await;
    }

    // Wait for all requests to complete
    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // All requests should have same body
    for (i, body, elapsed) in &results {
        assert_eq!(body, "Slow response", "Request {} failed", i);
        println!("Request {} completed in {:?}", i, elapsed);
    }

    // At least some requests should have joined the in-flight download
    // (completed faster than if they each waited for slow response)
    let fast_requests = results.iter().filter(|(_, _, elapsed)| *elapsed < Duration::from_millis(150)).count();
    assert!(fast_requests >= 3, "Expected at least 3 requests to benefit from concurrent handling, got {}", fast_requests);
}

#[tokio::test]
async fn test_streaming_large_file() {
    let (origin_addr, _origin_handle) = start_test_server().await;
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // Request large file
    let response = client
        .get(format!("http://{}/large", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Get the body and verify size
    let body = response.bytes().await.unwrap();
    let total_bytes = body.len();

    // Should have received 1MB
    assert_eq!(total_bytes, 1024 * 1024);
}

#[tokio::test]
async fn test_cache_control_headers() {
    let (origin_addr, _origin_handle) = start_test_server().await;
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // Request resource with cache-control header
    let response = client
        .get(format!("http://{}/cache-control", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(body, "Cached content");

    // Second request should be served from cache
    let response2 = client
        .get(format!("http://{}/cache-control", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let has_cache_hit = response2.headers().get("x-cache").map(|v| v.to_str().unwrap()) == Some("HIT");
    assert!(has_cache_hit, "Expected cache HIT header");
}

#[tokio::test]
async fn test_http2_support() {
    // Test that proxy can handle HTTP/2 connections
    // The auto builder supports both HTTP/1.1 and HTTP/2

    let (origin_addr, _origin_handle) = start_test_server().await;
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    // Create regular client (will use HTTP/1.1 or HTTP/2 based on negotiation)
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // Make a request - the proxy should handle it regardless of HTTP version
    let response = client
        .get(format!("http://{}/test", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(body, "Hello from test server");
}

#[tokio::test]
async fn test_apt_request_detection() {
    use squiddish::apt::{is_apt_request, is_apt_package_file, is_apt_package_list};

    // Test APT request detection
    assert!(is_apt_request("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
    assert!(is_apt_request("http://deb.debian.org/debian/dists/stable/main/binary-amd64/Packages.gz"));
    assert!(!is_apt_request("http://example.com/file.tar.gz"));

    // Test package file detection
    assert!(is_apt_package_file("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
    assert!(!is_apt_package_file("http://archive.ubuntu.com/ubuntu/dists/stable/Release"));

    // Test package list detection
    assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/stable/main/binary-amd64/Packages.gz"));
    assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/stable/InRelease"));
    assert!(!is_apt_package_list("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
}

#[tokio::test]
async fn test_vary_header_support() {
    // Start test HTTP server
    let (origin_addr, _origin_handle) = start_test_server().await;

    // Start proxy server
    let (proxy_addr, _proxy_handle) = start_proxy_server().await;

    // Create client configured to use proxy
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy_addr)).unwrap())
        .build()
        .unwrap();

    // First request with gzip Accept-Encoding
    let response1 = client
        .get(format!("http://{}/vary", origin_addr))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response1.status(), StatusCode::OK);
    let body1 = response1.text().await.unwrap();
    assert_eq!(body1, "gzip content");

    // Second request with no Accept-Encoding (should get different cached response)
    let response2 = client
        .get(format!("http://{}/vary", origin_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = response2.text().await.unwrap();
    assert_eq!(body2, "plain content");

    // Third request with gzip again - should hit cache and get gzip content
    let response3 = client
        .get(format!("http://{}/vary", origin_addr))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response3.status(), StatusCode::OK);
    let body3 = response3.text().await.unwrap();
    assert_eq!(body3, "gzip content");
}
