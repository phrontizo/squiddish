mod handler;
mod tunnel;
mod client;

use crate::cache::TieredCache;
use crate::config::Config;
use crate::error::Result;
use handler::ProxyHandler;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::net::TcpListener;

pub struct ProxyServer {
    config: Config,
    cache: Arc<TieredCache>,
}

impl ProxyServer {
    pub async fn new(config: Config) -> Result<Self> {
        // Initialize cache
        let cache = TieredCache::new(
            config.cache.memory_size,
            config.cache.cache_dir.clone(),
            config.cache.disk_size,
        )
        .await?;

        Ok(Self {
            config,
            cache: Arc::new(cache),
        })
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(&self.config.bind_addr).await?;
        tracing::info!("Proxy server listening on {}", self.config.bind_addr);

        let config = Arc::new(self.config.clone());

        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to accept connection: {}", e);
                    continue;
                }
            };

            tracing::debug!("New connection from {}", peer_addr);

            let cache = self.cache.clone();
            let config = config.clone();

            tokio::spawn(async move {
                let io = TokioIo::new(stream);

                let handler = ProxyHandler::new(cache, config, peer_addr);

                let service = service_fn(move |req| {
                    let handler = handler.clone();
                    async move { handler.handle(req).await }
                });

                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await
                {
                    tracing::error!("Error serving connection from {}: {}", peer_addr, err);
                }
            });
        }
    }
}
