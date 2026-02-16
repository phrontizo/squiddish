use crate::apt::is_apt_request;
use crate::cache::{CacheEntry, CacheKey, TieredCache};
use crate::config::Config;
use crate::error::{ProxyError, Result};
use crate::proxy::client::{collect_body, create_client, fetch_with_timeout, HttpClient};
use crate::proxy::tunnel::handle_connect_tunnel;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
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
    ) -> std::result::Result<Response<Full<Bytes>>, hyper::Error> {
        let result = self.handle_request(req).await;

        match result {
            Ok(response) => Ok(response),
            Err(e) => {
                tracing::error!("Proxy error: {}", e);
                Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from(format!("Proxy Error: {}", e))))
                    .unwrap())
            }
        }
    }

    async fn handle_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>> {
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
    ) -> Result<Response<Full<Bytes>>> {
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
            .body(Full::new(Bytes::new()))
            .unwrap())
    }

    async fn handle_cacheable_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>> {
        // Create cache key
        let cache_key = self.create_cache_key(&req);

        // Check cache
        if let Some(entry) = self.cache.get(&cache_key).await? {
            tracing::info!("Cache HIT: {}", req.uri());
            return Ok(build_response_from_cache(entry));
        }

        tracing::info!("Cache MISS: {}", req.uri());

        // Forward request
        let response = self.forward_request(req).await?;

        // Cache if cacheable
        if should_cache(&response) {
            if let Err(e) = self.cache_response(&cache_key, &response).await {
                tracing::warn!("Failed to cache response: {}", e);
            }
        }

        Ok(response)
    }

    async fn handle_passthrough_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>> {
        tracing::info!("Passthrough request: {} {}", req.method(), req.uri());
        self.forward_request(req).await
    }

    async fn forward_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>> {
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
            format!("1.1 squiddish").parse().unwrap(),
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
            .body(Full::new(body_bytes))
            .map_err(ProxyError::from)?;

        *resp.headers_mut() = filter_headers(headers);

        Ok(resp)
    }

    fn create_cache_key(&self, req: &Request<Incoming>) -> CacheKey {
        let uri = req.uri().to_string();
        let method = req.method().as_str();

        // Include Vary headers in cache key
        let vary_headers: Vec<(String, String)> = vec![];

        CacheKey::new(method, &uri, &vary_headers)
    }

    async fn cache_response(
        &self,
        key: &CacheKey,
        response: &Response<Full<Bytes>>,
    ) -> Result<()> {
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        // Get body as Bytes - Full<Bytes> wraps a single Bytes value
        // We collect all frames using BodyExt::collect
        let collected = response.body().clone().collect().await.map_err(|e| {
            ProxyError::Cache(format!("Failed to collect body: {}", e))
        })?;
        let data = collected.to_bytes();

        // Determine TTL based on request type with APT-specific optimizations
        let ttl_seconds = if is_apt_request(key.uri()) {
            // APT-specific TTL logic
            if crate::apt::is_apt_package_file(key.uri()) {
                // .deb files are immutable - use configured package TTL
                self.config.apt.package_ttl_seconds
            } else if crate::apt::is_apt_package_list(key.uri()) {
                // Package lists change frequently - use configured list TTL
                self.config.apt.list_ttl_seconds
            } else {
                // Other APT files - use configured other TTL
                self.config.apt.other_ttl_seconds
            }
        } else {
            self.config.cache.ttl_seconds
        };

        let entry = CacheEntry {
            data,
            headers,
            status: response.status().as_u16(),
            timestamp: SystemTime::now(),
            ttl_seconds,
        };

        self.cache.put(key.clone(), entry).await?;

        tracing::debug!("Cached response for {}", key.uri());

        Ok(())
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

fn should_cache(response: &Response<Full<Bytes>>) -> bool {
    let status = response.status();

    // Only cache successful responses
    if !status.is_success() {
        return false;
    }

    // Check Cache-Control headers
    if let Some(cache_control) = response.headers().get("cache-control") {
        if let Ok(value) = cache_control.to_str() {
            if value.contains("no-store") || value.contains("private") {
                return false;
            }
        }
    }

    true
}

fn build_response_from_cache(entry: CacheEntry) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(entry.status);

    for (name, value) in entry.headers {
        builder = builder.header(name, value);
    }

    // Add cache hit header
    builder = builder.header("x-cache", "HIT");

    builder.body(Full::new(entry.data)).unwrap()
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

    #[test]
    fn test_should_cache_success() {
        let response = Response::builder()
            .status(200)
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(should_cache(&response));
    }

    #[test]
    fn test_should_cache_no_store() {
        let response = Response::builder()
            .status(200)
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(!should_cache(&response));
    }

    #[test]
    fn test_should_cache_error() {
        let response = Response::builder()
            .status(404)
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(!should_cache(&response));
    }
}
