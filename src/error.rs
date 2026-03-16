use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] hyper::http::Error),

    #[error("Hyper error: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("Hyper util error: {0}")]
    HyperUtil(String),

    #[error("Invalid URI: {0}")]
    InvalidUri(String),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Tunnel error: {0}")]
    Tunnel(String),

    #[error("Request validation failed: {0}")]
    ValidationFailed(String),

    #[error("DNS lookup failed: {0}")]
    DnsError(String),

    #[error("Network error: {0}")]
    Network(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;
