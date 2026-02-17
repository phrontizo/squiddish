use crate::apt::is_apt_request;
use crate::cache::{CacheEntry, CacheKey, DownloadChunk, TieredCache};
use crate::config::Config;
use crate::error::{ProxyError, Result};
use crate::proxy::client::{collect_body, create_client, fetch_with_timeout, HttpClient};
use crate::proxy::streaming::StreamingBody;
use crate::proxy::tunnel::handle_connect_tunnel;
use bytes::Bytes;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode, Uri};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Clone)]
pub struct ProxyHandler {
    cache: Arc<TieredCache>,
    config: Arc<Config>,
    client: HttpClient,
    peer_addr: SocketAddr,
}

impl ProxyHandler {
    pub fn new(cache: Arc<TieredCache>, config: Arc<Config>, peer_addr: SocketAddr) -> Self {
        Self {
            cache,
            config,
            client: create_client(),
            peer_addr,
        }
    }

    pub async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> std::result::Result<Response<Either<Full<Bytes>, StreamingBody>>, hyper::Error> {
        let result = self.handle_request(req).await;

        match result {
            Ok(response) => Ok(response),
            Err(e) => {
                tracing::error!("Proxy error: {}", e);
                Ok(self.build_error_page(&e))
            }
        }
    }

    async fn handle_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        // Security validations
        self.validate_request(&req)?;

        // Handle CONNECT method for HTTPS tunneling
        if req.method() == Method::CONNECT {
            return self.handle_connect(req).await;
        }

        // Handle cacheable requests (GET, HEAD)
        if req.method() == Method::GET || req.method() == Method::HEAD {
            return self.handle_cacheable_request(req).await;
        }

        // Pass through other methods without caching
        self.handle_passthrough_request(req).await
    }

    fn validate_request(&self, req: &Request<Incoming>) -> Result<()> {
        // Validate host if restrictions are configured
        if !self.config.security.allowed_hosts.is_empty() {
            let host = req
                .uri()
                .host()
                .or_else(|| req.headers().get("host").and_then(|h| h.to_str().ok()))
                .ok_or_else(|| ProxyError::ValidationFailed("Missing host".to_string()))?;

            let allowed = self
                .config
                .security
                .allowed_hosts
                .iter()
                .any(|pattern| host.contains(pattern));

            if !allowed {
                return Err(ProxyError::ValidationFailed(format!(
                    "Host not allowed: {}",
                    host
                )));
            }
        }

        // Check blocked hosts
        if let Some(host) = req.uri().host() {
            for blocked in &self.config.security.blocked_hosts {
                if host.contains(blocked) {
                    return Err(ProxyError::ValidationFailed(format!(
                        "Host blocked: {}",
                        host
                    )));
                }
            }
        }

        // Validate URI
        if req.uri().scheme().is_none() && req.method() != Method::CONNECT {
            return Err(ProxyError::InvalidUri(
                "Request must include scheme".to_string(),
            ));
        }

        Ok(())
    }

    async fn handle_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        tracing::info!("CONNECT request to {}", req.uri());

        // Parse target host and port
        let uri = req.uri();
        let (host, port) = parse_connect_uri(uri)?;

        // Validate HTTPS port (security measure)
        if self.config.security.strict_https && port != 443 {
            return Err(ProxyError::Tunnel(format!(
                "Only port 443 allowed for CONNECT, got {}",
                port
            )));
        }

        let peer_addr = self.peer_addr;

        // Send 200 Connection Established
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    if let Err(e) =
                        handle_connect_tunnel(upgraded, host, port, peer_addr).await
                    {
                        tracing::error!("Tunnel error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Upgrade error: {}", e);
                }
            }
        });

        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Either::Left(Full::new(Bytes::new())))?)
    }

    async fn handle_cacheable_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        // Create cache key
        let cache_key = self.create_cache_key(&req);

        // Check cache
        let is_apt = crate::apt::is_apt_request(req.uri().to_string().as_str());
        let cache_type = if is_apt { "[APT]" } else { "[HTTP]" };

        if let Some(entry) = self.cache.get(&cache_key).await? {
            tracing::info!("{} Cache HIT: {}", cache_type, req.uri());
            return Ok(build_response_from_cache(entry));
        }

        tracing::info!("{} Cache MISS: {}", cache_type, req.uri());

        // Check if download is already in progress
        if let Some((receiver, initial_chunks)) = self.cache.inflight().join_download(&cache_key) {
            tracing::info!("{} Joining in-flight download: {}", cache_type, req.uri());
            // Return streaming response for concurrent request
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .body(Either::Right(StreamingBody::new(receiver, initial_chunks)))?);
        }

        // Start a new download with streaming
        self.handle_streaming_download(req, cache_key, is_apt).await
    }

    async fn handle_streaming_download(
        &self,
        req: Request<Incoming>,
        cache_key: CacheKey,
        _is_apt: bool,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        // Start the download and get broadcast sender
        let sender = self.cache.inflight().start_download(&cache_key);
        let receiver = sender.subscribe();

        // Clone necessary data for the background task
        let cache = self.cache.clone();
        let config = self.config.clone();
        let client = self.client.clone();
        let uri = req.uri().clone();

        // Spawn background task to fetch and broadcast
        tokio::spawn(async move {
            // Forward the request
            let result = Self::fetch_and_stream(
                client,
                req,
                &sender,
                &cache,
                &cache_key,
                config.security.max_body_size,
                config.security.timeout_seconds,
                config.clone(),
            ).await;

            match result {
                Ok(_) => {
                    tracing::debug!("Download completed successfully: {}", uri);
                    let _ = sender.send(DownloadChunk::Complete);
                }
                Err(e) => {
                    tracing::error!("Download failed: {} - {}", uri, e);
                    let _ = sender.send(DownloadChunk::Error(e.to_string()));
                }
            }

            // Mark download as complete
            cache.inflight().complete_download(&cache_key);
        });

        // Return streaming response immediately
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Either::Right(StreamingBody::new(receiver, vec![])))?)
    }

    async fn handle_passthrough_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        tracing::info!("Passthrough request: {} {}", req.method(), req.uri());
        self.forward_request_full(req).await
    }

    async fn fetch_and_stream(
        client: HttpClient,
        req: Request<Incoming>,
        sender: &tokio::sync::broadcast::Sender<DownloadChunk>,
        cache: &Arc<TieredCache>,
        cache_key: &CacheKey,
        max_body_size: u64,
        timeout_secs: u64,
        config: Arc<Config>,
    ) -> Result<()> {
        // Build new request with full URI
        let uri = req.uri().clone();
        let method = req.method().clone();
        let headers = req.headers().clone();

        // Collect request body
        let body_bytes = collect_body(req.into_body(), max_body_size).await?;

        // Create new request with absolute URI
        let mut new_req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Full::new(body_bytes))
            .map_err(ProxyError::from)?;

        // Copy headers (excluding hop-by-hop headers)
        *new_req.headers_mut() = filter_headers(headers);

        // Add Via header
        new_req.headers_mut().insert(
            "via",
            "1.1 squiddish".to_string().parse().unwrap(),
        );

        // Forward request
        let response = fetch_with_timeout(&client, new_req, timeout_secs).await?;

        let status = response.status();
        let response_headers = response.headers().clone();

        // Stream the response body
        let mut body = response.into_body();
        let mut accumulated_data = Vec::new();
        let mut total_size = 0u64;

        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(chunk) = frame.data_ref() {
                if total_size + chunk.len() as u64 > max_body_size {
                    return Err(ProxyError::ValidationFailed("Response body too large".to_string()));
                }

                let bytes = chunk.clone();
                total_size += bytes.len() as u64;

                // Add to accumulated data for caching
                accumulated_data.push(bytes.clone());

                // Add to inflight tracking for late joiners
                cache.inflight().add_chunk(cache_key, bytes.clone());

                // Broadcast to all listeners
                let _ = sender.send(DownloadChunk::Data(bytes));
            }
        }

        // Combine all chunks for caching
        let mut full_body = Vec::new();
        for chunk in accumulated_data {
            full_body.extend_from_slice(&chunk);
        }
        let body_bytes = Bytes::from(full_body);

        // Cache the complete response with proper TTL
        let headers_vec: Vec<(String, String)> = response_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        // Determine TTL based on APT detection or cache headers
        let ttl_seconds = if is_apt_request(cache_key.uri()) {
            if crate::apt::is_apt_package_file(cache_key.uri()) {
                config.apt.package_ttl_seconds
            } else if crate::apt::is_apt_package_list(cache_key.uri()) {
                config.apt.list_ttl_seconds
            } else {
                config.apt.other_ttl_seconds
            }
        } else {
            // For non-APT, parse cache control headers
            Self::parse_cache_control_headers(&response_headers)
                .unwrap_or(config.cache.ttl_seconds)
        };

        let entry = CacheEntry {
            data: body_bytes,
            headers: headers_vec,
            status: status.as_u16(),
            timestamp: SystemTime::now(),
            ttl_seconds,
        };

        let _ = cache.put(cache_key.clone(), entry).await;

        Ok(())
    }

    fn parse_cache_control_headers(headers: &hyper::HeaderMap) -> Option<u64> {
        // Check Cache-Control header
        if let Some(cache_control) = headers.get("cache-control") {
            if let Ok(value) = cache_control.to_str() {
                for directive in value.split(',') {
                    let directive = directive.trim();
                    if directive.starts_with("max-age=") {
                        if let Ok(seconds) = directive[8..].parse::<u64>() {
                            return Some(seconds);
                        }
                    }
                    if directive.starts_with("s-maxage=") {
                        if let Ok(seconds) = directive[9..].parse::<u64>() {
                            return Some(seconds);
                        }
                    }
                }
            }
        }

        // Check Expires header
        if let Some(expires) = headers.get("expires") {
            if let Ok(expires_str) = expires.to_str() {
                if let Ok(expires_time) = httpdate::parse_http_date(expires_str) {
                    let now = SystemTime::now();
                    if let Ok(duration) = expires_time.duration_since(now) {
                        return Some(duration.as_secs());
                    }
                }
            }
        }

        None
    }

    async fn forward_request_full(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        // Build new request with full URI
        let uri = req.uri().clone();
        let method = req.method().clone();
        let headers = req.headers().clone();

        // Collect request body
        let body_bytes = collect_body(req.into_body(), self.config.security.max_body_size).await?;

        // Create new request with absolute URI
        let mut new_req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Full::new(body_bytes))
            .map_err(ProxyError::from)?;

        // Copy headers (excluding hop-by-hop headers)
        *new_req.headers_mut() = filter_headers(headers);

        // Add Via header
        new_req.headers_mut().insert(
            "via",
            "1.1 squiddish".to_string().parse().unwrap(),
        );

        // Forward request
        let response = fetch_with_timeout(
            &self.client,
            new_req,
            self.config.security.timeout_seconds,
        )
        .await?;

        // Collect response body
        let status = response.status();
        let headers = response.headers().clone();
        let body_bytes = collect_body(response.into_body(), self.config.security.max_body_size).await?;

        // Build response
        let mut resp = Response::builder()
            .status(status)
            .body(Either::Left(Full::new(body_bytes)))
            .map_err(ProxyError::from)?;

        *resp.headers_mut() = filter_headers(headers);

        Ok(resp)
    }

    fn create_cache_key(&self, req: &Request<Incoming>) -> CacheKey {
        let uri = req.uri().to_string();
        let method = req.method().as_str();

        // For proper Vary support, we'd need to check if there's already a cached
        // response with Vary headers. For now, we include common vary headers
        // that are typically used (Accept-Encoding, Accept, User-Agent)
        let vary_headers: Vec<(String, String)> = req.headers()
            .iter()
            .filter_map(|(name, value)| {
                let name_str = name.as_str().to_lowercase();
                // Include common headers that servers typically vary on
                if name_str == "accept-encoding" || name_str == "accept" || name_str == "user-agent" {
                    Some((name.as_str().to_string(), value.to_str().unwrap_or("").to_string()))
                } else {
                    None
                }
            })
            .collect();

        CacheKey::new(method, &uri, &vary_headers)
    }

    fn build_error_page(&self, error: &ProxyError) -> Response<Either<Full<Bytes>, StreamingBody>> {
        let error_html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Squiddish Proxy Error</title>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            max-width: 800px;
            margin: 50px auto;
            padding: 20px;
            background: #f5f5f5;
        }}
        .error-container {{
            background: white;
            border-radius: 8px;
            padding: 30px;
            box-shadow: 0 2px 10px rgba(0,0,0,0.1);
        }}
        h1 {{
            color: #d32f2f;
            margin-top: 0;
        }}
        .error-code {{
            font-family: monospace;
            background: #f5f5f5;
            padding: 15px;
            border-radius: 4px;
            margin: 20px 0;
            word-wrap: break-word;
        }}
        .info {{
            color: #666;
            margin-top: 20px;
            padding-top: 20px;
            border-top: 1px solid #eee;
        }}
        .timestamp {{
            color: #999;
            font-size: 0.9em;
        }}
    </style>
</head>
<body>
    <div class="error-container">
        <h1>🦑 Squiddish Proxy Error</h1>
        <p>The proxy encountered an error while processing your request.</p>

        <div class="error-code">
            <strong>Error:</strong> {}
        </div>

        <div class="info">
            <p><strong>What happened?</strong></p>
            <p>The proxy was unable to complete your request. This could be due to:</p>
            <ul>
                <li>The remote server being unreachable</li>
                <li>Network connectivity issues</li>
                <li>Invalid request format</li>
                <li>Cache system errors</li>
            </ul>

            <p class="timestamp">Client: {} | Time: {}</p>
        </div>
    </div>
</body>
</html>"#,
            error,
            self.peer_addr,
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );

        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "text/html; charset=utf-8")
            .body(Either::Left(Full::new(Bytes::from(error_html))))
            .unwrap()
    }
}

fn parse_connect_uri(uri: &Uri) -> Result<(String, u16)> {
    let authority = uri
        .authority()
        .ok_or_else(|| ProxyError::InvalidUri("Missing authority in CONNECT".to_string()))?;

    let host_port = authority.as_str();
    let parts: Vec<&str> = host_port.split(':').collect();

    match parts.as_slice() {
        [host, port] => {
            let port = port
                .parse::<u16>()
                .map_err(|_| ProxyError::InvalidUri("Invalid port".to_string()))?;
            Ok((host.to_string(), port))
        }
        [host] => Ok((host.to_string(), 443)),
        _ => Err(ProxyError::InvalidUri("Invalid CONNECT URI".to_string())),
    }
}


fn build_response_from_cache(entry: CacheEntry) -> Response<Either<Full<Bytes>, StreamingBody>> {
    let mut builder = Response::builder().status(entry.status);

    for (name, value) in entry.headers {
        builder = builder.header(name, value);
    }

    // Add cache hit header
    builder = builder.header("x-cache", "HIT");

    builder.body(Either::Left(Full::new(entry.data))).unwrap()
}

fn filter_headers(mut headers: hyper::HeaderMap) -> hyper::HeaderMap {
    // Remove hop-by-hop headers
    headers.remove("connection");
    headers.remove("keep-alive");
    headers.remove("proxy-authenticate");
    headers.remove("proxy-authorization");
    headers.remove("te");
    headers.remove("trailers");
    headers.remove("transfer-encoding");
    headers.remove("upgrade");

    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_connect_uri() {
        let uri: Uri = "example.com:443".parse().unwrap();
        let (host, port) = parse_connect_uri(&uri).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

}
