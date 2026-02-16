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
    timeout(Duration::from_secs(timeout_secs), client.request(req))
        .await
        .map_err(|_| ProxyError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Request timed out"
        )))?
        .map_err(|e| ProxyError::HyperUtil(e.to_string()))
}

pub async fn collect_body(body: Incoming, max_size: u64) -> Result<Bytes> {
    let mut collected = Vec::new();
    let mut body = body;

    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(chunk) = frame.data_ref() {
            if collected.len() as u64 + chunk.len() as u64 > max_size {
                return Err(ProxyError::ValidationFailed(
                    "Response body too large".to_string(),
                ));
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
