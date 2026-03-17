use crate::apt::is_apt_request;
use crate::cache::{
    CacheEntry, CacheKey, DownloadAction, DownloadChunk, ResponseMeta, TieredCache,
};
use crate::config::Config;
use crate::error::{ProxyError, Result};
use crate::proxy::client::{collect_body, fetch_with_timeout, HttpClient};
use crate::proxy::streaming::StreamingBody;
use crate::proxy::tunnel::handle_connect_tunnel;
use bytes::Bytes;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode, Uri};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

/// RAII guard that calls `complete_download` on drop, ensuring cleanup
/// even if the background download task panics.
struct InflightGuard {
    cache: Arc<TieredCache>,
    key: CacheKey,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.cache.inflight().complete_download(&self.key);
    }
}

#[derive(Clone)]
pub struct ProxyHandler {
    cache: Arc<TieredCache>,
    config: Arc<Config>,
    client: HttpClient,
    peer_addr: SocketAddr,
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl ProxyHandler {
    pub fn new(
        cache: Arc<TieredCache>,
        config: Arc<Config>,
        client: HttpClient,
        peer_addr: SocketAddr,
        semaphore: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            cache,
            config,
            client,
            peer_addr,
            semaphore,
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
        // RFC 7230 §5.7.1: Detect request loops via Via header
        if has_via_loop(req.headers()) {
            return Err(ProxyError::ValidationFailed(
                "Request loop detected via Via header".to_string(),
            ));
        }

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
                .any(|pattern| host_matches(host, pattern));

            if !allowed {
                return Err(ProxyError::ValidationFailed(format!(
                    "Host not allowed: {}",
                    host
                )));
            }
        }

        // Check blocked hosts — require a host to be extractable when blocking is configured,
        // matching the allowed_hosts behavior to prevent bypass via missing host.
        if !self.config.security.blocked_hosts.is_empty() {
            let host = req
                .uri()
                .host()
                .or_else(|| req.headers().get("host").and_then(|h| h.to_str().ok()))
                .ok_or_else(|| ProxyError::ValidationFailed("Missing host".to_string()))?;

            for blocked in &self.config.security.blocked_hosts {
                if host_matches(host, blocked) {
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

        // Connect to target BEFORE sending 200 OK.
        // This prevents returning success for non-existent domains
        // and eliminates the double-connection overhead.
        let target_addr = format!("{}:{}", host, port);
        let connect_timeout = std::time::Duration::from_secs(self.config.security.timeout_seconds);
        let target_stream = tokio::time::timeout(
            connect_timeout,
            tokio::net::TcpStream::connect(&target_addr),
        )
        .await
        .map_err(|_| ProxyError::Tunnel(format!("Connection to {} timed out", target_addr)))?
        .map_err(|e| ProxyError::Tunnel(format!("Failed to connect to {}: {}", target_addr, e)))?;

        let peer_addr = self.peer_addr;
        let upgrade_timeout = std::time::Duration::from_secs(self.config.security.timeout_seconds);

        // Acquire a semaphore permit for the tunnel's lifetime. The HTTP connection
        // permit (in proxy/mod.rs) is released when serve_connection completes, but
        // the spawned tunnel continues running — without its own permit, tunnels
        // would bypass the connection limit.
        let tunnel_permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProxyError::Tunnel("Max connections reached".to_string()))?;

        // Now send 200 Connection Established (we know the target is reachable)
        // Pass the pre-established connection to the tunnel handler
        tokio::spawn(async move {
            let _tunnel_permit = tunnel_permit; // held for tunnel lifetime

            // Timeout the upgrade handshake to prevent holding the pre-established
            // target connection indefinitely if the client never completes the upgrade.
            match tokio::time::timeout(upgrade_timeout, hyper::upgrade::on(req)).await {
                Ok(Ok(upgraded)) => {
                    if let Err(e) =
                        handle_connect_tunnel(upgraded, target_stream, peer_addr, target_addr).await
                    {
                        tracing::error!("Tunnel error: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Upgrade error: {}", e);
                }
                Err(_) => {
                    tracing::warn!("Upgrade timed out for {}, dropping connection", target_addr);
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
        // Create cache key (stores the URI string, reused for APT detection)
        let cache_key = self.create_cache_key(&req);

        // Check cache (use cache_key.uri() to avoid redundant URI stringification)
        let is_apt = crate::apt::is_apt_request(cache_key.uri());
        let cache_type = if is_apt { "[APT]" } else { "[HTTP]" };

        if let Some(entry) = self.cache.get(&cache_key).await? {
            tracing::info!("{} Cache HIT: {}", cache_type, req.uri());
            return Ok(build_response_from_cache(entry));
        }

        tracing::info!("{} Cache MISS: {}", cache_type, req.uri());

        // Atomically join an existing download or start a new one.
        // This prevents a TOCTOU race where two concurrent requests both see
        // "no download" and both call start_download (overwriting each other's state).
        match self.cache.inflight().join_or_start_download(&cache_key) {
            DownloadAction::Joined(receiver, initial_chunks, mut meta_rx) => {
                tracing::info!("{} Joining in-flight download: {}", cache_type, req.uri());

                let meta_result = meta_rx
                    .wait_for(|v| v.is_some())
                    .await
                    .map_err(|_| {
                        ProxyError::Network("Download failed before headers received".to_string())
                    })?
                    .clone()
                    .unwrap();

                let meta = meta_result.map_err(ProxyError::Network)?;
                build_streaming_response(meta, receiver, initial_chunks)
            }
            DownloadAction::Started(sender, meta_rx) => {
                self.handle_streaming_download(req, cache_key, sender, meta_rx)
                    .await
            }
        }
    }

    async fn handle_streaming_download(
        &self,
        req: Request<Incoming>,
        cache_key: CacheKey,
        sender: tokio::sync::broadcast::Sender<DownloadChunk>,
        mut meta_rx: tokio::sync::watch::Receiver<Option<crate::cache::ResponseMetaResult>>,
    ) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
        // Subscribe BEFORE spawning so we don't miss any chunks
        let receiver = sender.subscribe();

        // Clone necessary data for the background task
        let cache = self.cache.clone();
        let config = self.config.clone();
        let client = self.client.clone();
        let uri = req.uri().clone();

        // Spawn background task to fetch and broadcast
        tokio::spawn(async move {
            // Guard ensures complete_download is called even if the task panics
            let _guard = InflightGuard {
                cache: cache.clone(),
                key: cache_key.clone(),
            };

            // Forward the request
            let result =
                Self::fetch_and_stream(client, req, &sender, &cache, &cache_key, config.clone())
                    .await;

            match result {
                Ok(_) => {
                    tracing::debug!("Download completed successfully: {}", uri);
                    let _ = sender.send(DownloadChunk::Complete);
                }
                Err(e) => {
                    tracing::error!("Download failed: {} - {}", uri, e);
                    // Propagate error through meta channel so waiters get the actual message
                    cache
                        .inflight()
                        .set_response_error(&cache_key, e.to_string());
                    let _ = sender.send(DownloadChunk::Error(e.to_string()));
                }
            }
        });

        // Wait for response metadata (status + headers) from the background task
        let meta_result = meta_rx
            .wait_for(|v| v.is_some())
            .await
            .map_err(|_| ProxyError::Network("Request failed".to_string()))?
            .clone()
            .unwrap();

        let meta = meta_result.map_err(ProxyError::Network)?;
        build_streaming_response(meta, receiver, vec![])
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
        config: Arc<Config>,
    ) -> Result<()> {
        let max_body_size = config.security.max_body_size;
        let timeout_secs = config.security.timeout_seconds;
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

        // RFC 7230 §5.7.1: proxies MUST append (not replace) Via header
        new_req.headers_mut().append(
            "via",
            hyper::header::HeaderValue::from_static("1.1 squiddish"),
        );

        // Forward request
        let response = fetch_with_timeout(&client, new_req, timeout_secs).await?;

        let status = response.status();
        // Filter hop-by-hop headers before storing — prevents replaying
        // Transfer-Encoding, Connection, etc. from cache to clients.
        let response_headers = filter_headers(response.headers().clone());

        // Convert headers once — reused for both response metadata and caching
        let headers_vec: Vec<(String, String)> = response_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        // Publish response metadata so both initiator and joiners get correct headers
        cache.inflight().set_response_meta(
            cache_key,
            ResponseMeta {
                status: status.as_u16(),
                headers: headers_vec.clone(),
            },
        );

        // Stream the response body with a per-chunk timeout.
        // Without this, a stalling upstream would hang all joined clients indefinitely.
        let mut body = response.into_body();
        // Accumulate directly into a single buffer to avoid doubling peak memory
        // (previously: Vec<Bytes> chunks + separate Vec<u8> copy at the end).
        let cacheable = should_cache_response(status, &response_headers);
        let mut cache_buf: Vec<u8> = Vec::new();
        let mut total_size = 0u64;
        let chunk_timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            let frame = match tokio::time::timeout(chunk_timeout, body.frame()).await {
                Ok(Some(frame)) => frame?,
                Ok(None) => break, // Body complete
                Err(_) => {
                    return Err(ProxyError::Network(
                        "Upstream body read timed out".to_string(),
                    ));
                }
            };
            if let Some(chunk) = frame.data_ref() {
                if total_size + chunk.len() as u64 > max_body_size {
                    return Err(ProxyError::ValidationFailed(
                        "Response body too large".to_string(),
                    ));
                }

                let bytes = chunk.clone();
                total_size += bytes.len() as u64;

                // Accumulate directly for caching (avoids separate chunk list + copy)
                if cacheable {
                    cache_buf.extend_from_slice(&bytes);
                }

                // Add to inflight tracking for late joiners
                cache.inflight().add_chunk(cache_key, bytes.clone());

                // Broadcast to all listeners
                let _ = sender.send(DownloadChunk::Data(bytes));
            }
        }

        if !cacheable {
            tracing::debug!(
                "Response not cacheable: status={}, uri={}",
                status,
                cache_key.uri()
            );
            return Ok(());
        }

        let body_bytes = Bytes::from(cache_buf);

        // Determine TTL based on APT detection or cache headers
        let ttl_seconds = if config.apt.enabled && is_apt_request(cache_key.uri()) {
            if crate::apt::is_apt_package_file(cache_key.uri()) {
                config.apt.package_ttl_seconds
            } else if crate::apt::is_apt_package_list(cache_key.uri()) {
                config.apt.list_ttl_seconds
            } else {
                config.apt.other_ttl_seconds
            }
        } else {
            // For non-APT, parse cache control headers
            Self::parse_cache_control_headers(&response_headers).unwrap_or(config.cache.ttl_seconds)
        };

        let entry = CacheEntry {
            data: body_bytes,
            headers: headers_vec,
            status: status.as_u16(),
            timestamp: SystemTime::now(),
            ttl_seconds,
        };

        if let Err(e) = cache.put(cache_key.clone(), entry).await {
            tracing::warn!("Failed to cache response for {}: {}", cache_key.uri(), e);
        }

        Ok(())
    }

    fn parse_cache_control_headers(headers: &hyper::HeaderMap) -> Option<u64> {
        let mut max_age = None;
        let mut s_maxage = None;

        // RFC 7230: multiple headers with the same name must all be processed
        for cache_control in headers.get_all("cache-control") {
            if let Ok(value) = cache_control.to_str() {
                for directive in value.split(',') {
                    // RFC 7234: Cache directives are case-insensitive
                    let directive = directive.trim().to_ascii_lowercase();
                    if let Some(val) = directive.strip_prefix("s-maxage=") {
                        if let Ok(seconds) = val.parse::<u64>() {
                            s_maxage = Some(seconds);
                        }
                    } else if let Some(val) = directive.strip_prefix("max-age=") {
                        if let Ok(seconds) = val.parse::<u64>() {
                            max_age = Some(seconds);
                        }
                    }
                }
            }
        }

        // s-maxage takes precedence over max-age for shared caches (RFC 7234)
        // Return None for zero TTL — entry would be immediately stale, wasting I/O
        if let Some(ttl) = s_maxage {
            return if ttl > 0 { Some(ttl) } else { None };
        }
        if let Some(ttl) = max_age {
            return if ttl > 0 { Some(ttl) } else { None };
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

        // RFC 7230 §5.7.1: proxies MUST append (not replace) Via header
        new_req.headers_mut().append(
            "via",
            hyper::header::HeaderValue::from_static("1.1 squiddish"),
        );

        // Forward request
        let response =
            fetch_with_timeout(&self.client, new_req, self.config.security.timeout_seconds).await?;

        // Collect response body
        let status = response.status();
        let headers = response.headers().clone();
        let body_bytes =
            collect_body(response.into_body(), self.config.security.max_body_size).await?;

        // Build response
        let mut resp = Response::builder()
            .status(status)
            .body(Either::Left(Full::new(body_bytes)))
            .map_err(ProxyError::from)?;

        *resp.headers_mut() = filter_headers(headers);
        // RFC 7230 §5.7.1: proxies MUST add a Via header to responses
        resp.headers_mut().append(
            "via",
            hyper::header::HeaderValue::from_static("1.1 squiddish"),
        );

        Ok(resp)
    }

    fn create_cache_key(&self, req: &Request<Incoming>) -> CacheKey {
        let uri = req.uri().to_string();
        let method = req.method().as_str();

        // Include Accept-Encoding in cache key since servers commonly vary on it.
        // User-Agent is excluded to avoid massive cache fragmentation.
        let vary_headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                // hyper normalizes header names to lowercase, so no .to_lowercase() needed
                if name.as_str() == "accept-encoding" {
                    Some((
                        name.as_str().to_string(),
                        value.to_str().unwrap_or("").to_string(),
                    ))
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

            <p class="timestamp">Time: {}</p>
        </div>
    </div>
</body>
</html>"#,
            html_escape(&error.to_string()),
            httpdate::fmt_http_date(SystemTime::now())
        );

        let status = match error {
            ProxyError::ValidationFailed(_) | ProxyError::InvalidUri(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };

        let body = Bytes::from(error_html);
        Response::builder()
            .status(status)
            .header("content-type", "text/html; charset=utf-8")
            .header("content-length", body.len().to_string())
            .body(Either::Left(Full::new(body)))
            .unwrap()
    }
}

/// Check if a response should be cached based on status and headers.
/// Uses `get_all()` per RFC 7230: multiple headers with the same name must all be processed.
fn should_cache_response(status: StatusCode, headers: &hyper::HeaderMap) -> bool {
    // Only cache successful responses
    if !status.is_success() {
        return false;
    }

    // Check Cache-Control directives (all header values)
    for cc in headers.get_all("cache-control") {
        if let Ok(value) = cc.to_str() {
            for directive in value.split(',') {
                let directive = directive.trim();
                if directive.eq_ignore_ascii_case("no-store")
                    || directive.eq_ignore_ascii_case("no-cache")
                    || directive.eq_ignore_ascii_case("private")
                {
                    return false;
                }
            }
        }
    }

    // Check Pragma: no-cache (HTTP/1.0 compatibility)
    // Use token-level matching to avoid false positives on values like "x-no-cache-ext"
    for pragma in headers.get_all("pragma") {
        if let Ok(value) = pragma.to_str() {
            for token in value.split(',') {
                if token.trim().eq_ignore_ascii_case("no-cache") {
                    return false;
                }
            }
        }
    }

    // Do not cache responses that Vary on sensitive headers.
    // The cache key only includes Accept-Encoding; if the upstream varies on
    // Authorization, Cookie, or *, caching would serve one user's response
    // to another (cache poisoning / data leak).
    for vary in headers.get_all("vary") {
        if let Ok(value) = vary.to_str() {
            for field in value.split(',') {
                let field = field.trim();
                if field == "*"
                    || field.eq_ignore_ascii_case("authorization")
                    || field.eq_ignore_ascii_case("cookie")
                {
                    return false;
                }
            }
        }
    }

    true
}

/// Escape a string for safe inclusion in HTML content.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Strip port from a host string (e.g., "example.com:8080" -> "example.com").
/// Handles IPv4/hostname with optional port, IPv6 bracket notation from Host headers
/// (e.g., "[::1]:8080"), and bare IPv6 from uri.host() (e.g., "::1").
fn strip_host_port(host: &str) -> &str {
    // IPv6 in brackets (from Host header): [::1]:port or [::1]
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map(|(addr, _)| addr).unwrap_or(rest);
    }
    // Bare IPv6 (from uri.host()): contains multiple colons, not host:port
    if host.bytes().filter(|&b| b == b':').count() > 1 {
        return host;
    }
    // IPv4 or hostname with optional port
    host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
}

/// RFC 7230 §5.7.1: Detect request loops by checking if the Via header
/// already contains our pseudonym ("1.1 squiddish"). Prevents infinite
/// forwarding loops when the proxy is accidentally configured to proxy to itself.
fn has_via_loop(headers: &hyper::HeaderMap) -> bool {
    for via in headers.get_all("via") {
        if let Ok(value) = via.to_str() {
            for entry in value.split(',') {
                if entry.trim().eq_ignore_ascii_case("1.1 squiddish") {
                    return true;
                }
            }
        }
    }
    false
}

/// Match a hostname against a pattern using exact match or subdomain suffix.
/// Pattern "example.com" matches "example.com" and "sub.example.com"
/// but NOT "evil-example.com". Strips port from host before matching.
/// Comparison is case-insensitive per RFC 4343 (DNS Name Case Insensitivity).
fn host_matches(host: &str, pattern: &str) -> bool {
    let host = strip_host_port(host);
    host.eq_ignore_ascii_case(pattern)
        || (host.len() > pattern.len()
            && host[host.len() - pattern.len()..].eq_ignore_ascii_case(pattern)
            && host.as_bytes()[host.len() - pattern.len() - 1] == b'.')
}

fn parse_connect_uri(uri: &Uri) -> Result<(String, u16)> {
    let authority = uri
        .authority()
        .ok_or_else(|| ProxyError::InvalidUri("Missing authority in CONNECT".to_string()))?;

    let host_port = authority.as_str();

    // Handle IPv6 addresses in bracket notation: [::1]:443
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some((ipv6, after_bracket)) = rest.split_once(']') {
            let port = if let Some(port_str) = after_bracket.strip_prefix(':') {
                port_str
                    .parse::<u16>()
                    .map_err(|_| ProxyError::InvalidUri("Invalid port".to_string()))?
            } else {
                443
            };
            return Ok((format!("[{}]", ipv6), port));
        }
        return Err(ProxyError::InvalidUri("Malformed IPv6 address".to_string()));
    }

    // IPv4 or hostname: split on last colon for host:port
    match host_port.rsplit_once(':') {
        Some((host, port_str)) => {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| ProxyError::InvalidUri("Invalid port".to_string()))?;
            Ok((host.to_string(), port))
        }
        None => Ok((host_port.to_string(), 443)),
    }
}

/// Build a streaming response from upstream metadata and a broadcast receiver.
fn build_streaming_response(
    meta: ResponseMeta,
    receiver: tokio::sync::broadcast::Receiver<DownloadChunk>,
    initial_chunks: Vec<Bytes>,
) -> Result<Response<Either<Full<Bytes>, StreamingBody>>> {
    let status = StatusCode::from_u16(meta.status).unwrap_or_else(|_| {
        tracing::warn!(
            "Invalid upstream status code {}, falling back to 200",
            meta.status
        );
        StatusCode::OK
    });
    let mut builder = Response::builder().status(status);
    for (name, value) in &meta.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    // RFC 7230 §5.7.1: proxies MUST add a Via header to responses
    builder = builder.header("via", "1.1 squiddish");
    Ok(builder.body(Either::Right(StreamingBody::new(receiver, initial_chunks)))?)
}

fn build_response_from_cache(entry: CacheEntry) -> Response<Either<Full<Bytes>, StreamingBody>> {
    // Validate status code first; fall back to 200 if corrupted (e.g., from tampered disk cache)
    let status = StatusCode::from_u16(entry.status).unwrap_or_else(|_| {
        tracing::warn!(
            "Corrupted cache entry status {}, falling back to 200",
            entry.status
        );
        StatusCode::OK
    });

    let mut builder = Response::builder().status(status);

    for (name, value) in &entry.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }

    // Add cache hit header
    builder = builder.header("x-cache", "HIT");
    // RFC 7230 §5.7.1: proxies MUST add a Via header to responses
    builder = builder.header("via", "1.1 squiddish");

    // Calculate remaining TTL
    if let Ok(elapsed) = entry.timestamp.elapsed() {
        let remaining_ttl = entry.ttl_seconds.saturating_sub(elapsed.as_secs());
        builder = builder.header("x-cache-ttl", remaining_ttl.to_string());
    }

    // Status is pre-validated above, so this should always succeed
    builder
        .body(Either::Left(Full::new(entry.data)))
        .unwrap_or_else(|_| Response::new(Either::Left(Full::new(Bytes::new()))))
}

fn filter_headers(mut headers: hyper::HeaderMap) -> hyper::HeaderMap {
    // Per RFC 7230 Section 6.1: the Connection header can list additional
    // hop-by-hop header names that must be removed before forwarding.
    let extra_hop_by_hop: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_lowercase())
        .collect();

    for name in &extra_hop_by_hop {
        headers.remove(name.as_str());
    }

    // Remove well-known hop-by-hop headers
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

    #[test]
    fn test_parse_connect_uri_default_port() {
        let uri: Uri = "example.com".parse().unwrap();
        let (host, port) = parse_connect_uri(&uri).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_parse_connect_uri_custom_port() {
        let uri: Uri = "example.com:8443".parse().unwrap();
        let (host, port) = parse_connect_uri(&uri).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
    }

    #[test]
    fn test_parse_connect_uri_ipv6() {
        let uri: Uri = "[::1]:443".parse().unwrap();
        let (host, port) = parse_connect_uri(&uri).unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_parse_connect_uri_ipv6_full() {
        let uri: Uri = "[2001:db8::1]:8443".parse().unwrap();
        let (host, port) = parse_connect_uri(&uri).unwrap();
        assert_eq!(host, "[2001:db8::1]");
        assert_eq!(port, 8443);
    }

    #[test]
    fn test_host_matches() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("sub.example.com", "example.com"));
        assert!(host_matches("deep.sub.example.com", "example.com"));
        assert!(!host_matches("evil-example.com", "example.com"));
        assert!(!host_matches("example.com.attacker.org", "example.com"));
        // Edge case: pattern longer than host
        assert!(!host_matches("com", "example.com"));
        // Host with port should still match
        assert!(host_matches("example.com:8080", "example.com"));
        assert!(host_matches("sub.example.com:443", "example.com"));
        assert!(!host_matches("evil-example.com:8080", "example.com"));
    }

    #[test]
    fn test_strip_host_port_ipv6() {
        // Bare IPv6 from uri.host() (brackets already stripped)
        assert_eq!(strip_host_port("::1"), "::1");
        assert_eq!(strip_host_port("2001:db8::1"), "2001:db8::1");
        // IPv6 bracket notation from Host header
        assert_eq!(strip_host_port("[::1]:8080"), "::1");
        assert_eq!(strip_host_port("[::1]"), "::1");
        assert_eq!(strip_host_port("[2001:db8::1]:443"), "2001:db8::1");
        // Regular IPv4/hostname unchanged
        assert_eq!(strip_host_port("example.com:8080"), "example.com");
        assert_eq!(strip_host_port("example.com"), "example.com");
        assert_eq!(strip_host_port("127.0.0.1:3128"), "127.0.0.1");
    }

    #[test]
    fn test_host_matches_ipv6() {
        // Bare IPv6 from uri.host()
        assert!(host_matches("::1", "::1"));
        assert!(host_matches("2001:db8::1", "2001:db8::1"));
        assert!(!host_matches("::1", "::2"));
        // Bracketed IPv6 from Host header
        assert!(host_matches("[::1]:8080", "::1"));
        assert!(host_matches("[2001:db8::1]:443", "2001:db8::1"));
    }

    #[test]
    fn test_host_matches_case_insensitive() {
        // RFC 4343: DNS names are case-insensitive
        assert!(host_matches("Example.COM", "example.com"));
        assert!(host_matches("example.com", "Example.COM"));
        assert!(host_matches("SUB.EXAMPLE.COM", "example.com"));
        assert!(host_matches("sub.Example.Com", "example.com"));
        assert!(host_matches("Sub.Example.Com:8080", "example.com"));
        assert!(!host_matches("evil-Example.COM", "example.com"));
    }

    #[test]
    fn test_should_cache_response() {
        let mut headers = hyper::HeaderMap::new();
        assert!(should_cache_response(StatusCode::OK, &headers));

        // Non-2xx should not be cached
        assert!(!should_cache_response(StatusCode::NOT_FOUND, &headers));
        assert!(!should_cache_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &headers
        ));

        // no-store
        headers.insert("cache-control", "no-store".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // no-cache
        headers.insert("cache-control", "no-cache".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // private
        headers.insert("cache-control", "private, max-age=300".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Normal cacheable response
        headers.insert("cache-control", "public, max-age=3600".parse().unwrap());
        assert!(should_cache_response(StatusCode::OK, &headers));
    }

    #[test]
    fn test_should_cache_response_multiple_headers() {
        // RFC 7230: multiple headers with the same name must all be processed
        let mut headers = hyper::HeaderMap::new();

        // no-store in a second Cache-Control header must prevent caching
        headers.append("cache-control", "public, max-age=3600".parse().unwrap());
        headers.append("cache-control", "no-store".parse().unwrap());
        assert!(
            !should_cache_response(StatusCode::OK, &headers),
            "no-store in second Cache-Control header should prevent caching"
        );

        // Vary: Authorization in a second Vary header must prevent caching
        let mut headers = hyper::HeaderMap::new();
        headers.append("vary", "Accept-Encoding".parse().unwrap());
        headers.append("vary", "Authorization".parse().unwrap());
        assert!(
            !should_cache_response(StatusCode::OK, &headers),
            "Authorization in second Vary header should prevent caching"
        );
    }

    #[test]
    fn test_parse_cache_control_multiple_headers() {
        let mut headers = hyper::HeaderMap::new();

        // s-maxage in a separate header should still be found
        headers.append("cache-control", "public".parse().unwrap());
        headers.append("cache-control", "s-maxage=600".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(600)
        );
    }

    #[test]
    fn test_s_maxage_precedence() {
        let mut headers = hyper::HeaderMap::new();

        // s-maxage should take precedence over max-age
        headers.insert("cache-control", "max-age=0, s-maxage=3600".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(3600)
        );

        // max-age alone
        headers.insert("cache-control", "max-age=300".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(300)
        );

        // s-maxage alone
        headers.insert("cache-control", "s-maxage=600".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(600)
        );

        // Case-insensitive per RFC 7234
        headers.insert("cache-control", "Max-Age=120".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(120)
        );

        headers.insert("cache-control", "S-MAXAGE=900".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(900)
        );
    }

    #[test]
    fn test_should_cache_response_pragma_no_cache() {
        let mut headers = hyper::HeaderMap::new();

        // Pragma: no-cache should prevent caching (HTTP/1.0 compatibility)
        headers.insert("pragma", "no-cache".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Other Pragma values should not prevent caching
        headers.remove("pragma");
        headers.insert("pragma", "something-else".parse().unwrap());
        assert!(should_cache_response(StatusCode::OK, &headers));

        // Pragma values containing "no-cache" as substring should NOT prevent caching
        headers.insert("pragma", "x-no-cache-extension".parse().unwrap());
        assert!(
            should_cache_response(StatusCode::OK, &headers),
            "Pragma substring match should not prevent caching"
        );

        // Pragma with multiple comma-separated tokens including no-cache
        headers.insert("pragma", "x-custom, no-cache".parse().unwrap());
        assert!(
            !should_cache_response(StatusCode::OK, &headers),
            "Pragma no-cache token in comma-separated list should prevent caching"
        );
    }

    #[test]
    fn test_should_not_cache_vary_authorization() {
        let mut headers = hyper::HeaderMap::new();

        // Vary: Accept-Encoding is fine (included in cache key)
        headers.insert("vary", "Accept-Encoding".parse().unwrap());
        assert!(should_cache_response(StatusCode::OK, &headers));

        // Vary: Authorization must prevent caching (not in cache key)
        headers.insert("vary", "Authorization".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Vary: Cookie must prevent caching
        headers.insert("vary", "Cookie".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Vary: * must prevent caching
        headers.insert("vary", "*".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Vary with mixed headers including Authorization
        headers.insert("vary", "Accept-Encoding, Authorization".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));

        // Case insensitive
        headers.insert("vary", "authorization".parse().unwrap());
        assert!(!should_cache_response(StatusCode::OK, &headers));
    }

    #[test]
    fn test_zero_ttl_returns_none() {
        let mut headers = hyper::HeaderMap::new();

        // max-age=0 should return None (entry would be immediately stale)
        headers.insert("cache-control", "max-age=0".parse().unwrap());
        assert_eq!(ProxyHandler::parse_cache_control_headers(&headers), None);

        // s-maxage=0 should also return None
        headers.insert("cache-control", "s-maxage=0".parse().unwrap());
        assert_eq!(ProxyHandler::parse_cache_control_headers(&headers), None);

        // s-maxage=0 with non-zero max-age: s-maxage takes precedence, returns None
        headers.insert("cache-control", "max-age=300, s-maxage=0".parse().unwrap());
        assert_eq!(ProxyHandler::parse_cache_control_headers(&headers), None);
    }

    #[test]
    fn test_expires_header_ttl() {
        let mut headers = hyper::HeaderMap::new();

        // Expires in the future should return a positive TTL
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
        headers.insert("expires", httpdate::fmt_http_date(future).parse().unwrap());
        let ttl = ProxyHandler::parse_cache_control_headers(&headers);
        assert!(ttl.is_some());
        // Allow some tolerance for test execution time
        let ttl_val = ttl.unwrap();
        assert!(
            (598..=601).contains(&ttl_val),
            "Expected ~600, got {}",
            ttl_val
        );

        // Expires in the past should return None
        headers.insert("expires", "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap());
        assert_eq!(ProxyHandler::parse_cache_control_headers(&headers), None);

        // Cache-Control max-age takes precedence over Expires
        headers.insert("cache-control", "max-age=120".parse().unwrap());
        assert_eq!(
            ProxyHandler::parse_cache_control_headers(&headers),
            Some(120)
        );
    }

    #[test]
    fn test_filter_headers_removes_hop_by_hop() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("proxy-authorization", "Basic abc123".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("x-custom", "preserved".parse().unwrap());

        let filtered = filter_headers(headers);

        // Hop-by-hop headers should be removed
        assert!(filtered.get("connection").is_none());
        assert!(filtered.get("keep-alive").is_none());
        assert!(filtered.get("proxy-authorization").is_none());
        assert!(filtered.get("transfer-encoding").is_none());

        // End-to-end headers should be preserved
        assert_eq!(filtered.get("content-type").unwrap(), "text/html");
        assert_eq!(filtered.get("x-custom").unwrap(), "preserved");
    }

    #[test]
    fn test_filter_headers_connection_declared_hop_by_hop() {
        // RFC 7230 Section 6.1: Connection header can declare additional hop-by-hop headers
        let mut headers = hyper::HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("connection", "X-Custom-Hop, X-Another".parse().unwrap());
        headers.insert("x-custom-hop", "should-be-removed".parse().unwrap());
        headers.insert("x-another", "also-removed".parse().unwrap());
        headers.insert("x-normal", "preserved".parse().unwrap());

        let filtered = filter_headers(headers);

        assert!(filtered.get("connection").is_none());
        assert!(
            filtered.get("x-custom-hop").is_none(),
            "Connection-declared hop-by-hop header should be removed"
        );
        assert!(
            filtered.get("x-another").is_none(),
            "Connection-declared hop-by-hop header should be removed"
        );
        assert_eq!(filtered.get("x-normal").unwrap(), "preserved");
        assert_eq!(filtered.get("content-type").unwrap(), "text/html");
    }

    #[test]
    fn test_detect_via_loop() {
        // "1.1 squiddish" in Via header should be detected as a loop
        assert!(has_via_loop(&{
            let mut h = hyper::HeaderMap::new();
            h.insert("via", "1.1 squiddish".parse().unwrap());
            h
        }));

        // Other proxy names should NOT be detected as a loop
        assert!(!has_via_loop(&{
            let mut h = hyper::HeaderMap::new();
            h.insert("via", "1.1 other-proxy".parse().unwrap());
            h
        }));

        // Multiple Via entries with squiddish among them
        assert!(has_via_loop(&{
            let mut h = hyper::HeaderMap::new();
            h.insert("via", "1.1 other-proxy, 1.1 squiddish".parse().unwrap());
            h
        }));

        // Multiple Via headers (append) with squiddish in second
        assert!(has_via_loop(&{
            let mut h = hyper::HeaderMap::new();
            h.append("via", "1.1 first-proxy".parse().unwrap());
            h.append("via", "1.1 squiddish".parse().unwrap());
            h
        }));

        // No Via header at all
        assert!(!has_via_loop(&hyper::HeaderMap::new()));

        // Substring that contains "squiddish" but isn't the exact pseudonym
        assert!(!has_via_loop(&{
            let mut h = hyper::HeaderMap::new();
            h.insert("via", "1.1 not-squiddish-proxy".parse().unwrap());
            h
        }));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("hello"), "hello");
        assert_eq!(
            html_escape("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape(r#"he said "hi""#), "he said &quot;hi&quot;");
        assert_eq!(html_escape("it's"), "it&#x27;s");
        // Combined: a URI with HTML injection
        assert_eq!(
            html_escape("http://<evil>.com/path?q=1&r=2"),
            "http://&lt;evil&gt;.com/path?q=1&amp;r=2"
        );
    }
}
