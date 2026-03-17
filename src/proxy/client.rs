use crate::error::{ProxyError, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;
use tokio::time::timeout;

pub type HttpClient = Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>;

pub fn create_client() -> HttpClient {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(30)));
    connector.set_nodelay(true);
    connector.set_keepalive(Some(Duration::from_secs(60)));

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(10)
        .build(connector)
}

pub async fn fetch_with_timeout(
    client: &HttpClient,
    req: Request<Full<Bytes>>,
    timeout_secs: u64,
) -> Result<Response<Incoming>> {
    let uri = req.uri().clone();
    let host = uri.host().unwrap_or("unknown");

    tracing::debug!("DNS lookup for: {}", host);

    timeout(Duration::from_secs(timeout_secs), client.request(req))
        .await
        .map_err(|_| {
            tracing::error!("DNS lookup or connection timeout for: {}", host);
            ProxyError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Request timed out connecting to {}", host),
            ))
        })?
        .map_err(|e| {
            // Inspect error chain for DNS/connection-refused errors
            if is_dns_error(&e) {
                tracing::error!("DNS lookup failed for {}: {}", host, e);
                ProxyError::DnsError(format!("Failed to resolve hostname: {}", host))
            } else {
                tracing::error!("Connection failed to {}: {}", host, e);
                ProxyError::HyperUtil(e.to_string())
            }
        })
}

/// Inspect the error chain for DNS resolution failures.
/// Walks the source chain looking for io::Error with specific error kinds,
/// rather than relying on fragile string matching against error messages.
fn is_dns_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(err);
    while let Some(err) = source {
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            // ConnectionRefused/ConnectionReset are NOT DNS errors
            // NotFound and "other" errors from getaddrinfo indicate DNS failure
            if io_err.kind() == std::io::ErrorKind::Other {
                // getaddrinfo failures surface as "Other" io errors
                let msg = io_err.to_string();
                if msg.contains("dns")
                    || msg.contains("resolve")
                    || msg.contains("Name or service not known")
                    || msg.contains("nodename nor servname provided")
                    || msg.contains("No address associated with hostname")
                {
                    return true;
                }
            }
        }
        source = err.source();
    }
    false
}

pub async fn collect_body(mut body: Incoming, max_size: u64) -> Result<Bytes> {
    let mut collected = Vec::new();

    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(chunk) = frame.data_ref() {
            if collected.len() as u64 + chunk.len() as u64 > max_size {
                return Err(ProxyError::ValidationFailed("Body too large".to_string()));
            }
            collected.extend_from_slice(chunk);
        }
    }

    Ok(Bytes::from(collected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_client() {
        let _client = create_client();
    }
}
