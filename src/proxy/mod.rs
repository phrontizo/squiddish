mod client;
mod handler;
mod streaming;
mod tunnel;

use crate::cache::TieredCache;
use crate::config::Config;
use crate::error::Result;
use client::create_client;
use handler::ProxyHandler;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

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
        self.serve(listener).await
    }

    /// Serve using a pre-bound listener (used by tests to avoid port races)
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        tracing::info!("Proxy server listening on {}", listener.local_addr()?);

        let config = Arc::new(self.config.clone());
        let semaphore = Arc::new(Semaphore::new(config.security.max_connections));
        // Create a single HTTP client shared across all connections for proper connection pooling
        let shared_client = create_client();

        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Shutdown signal received");
        };
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, peer_addr) = match result {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("Failed to accept connection: {}", e);
                            continue;
                        }
                    };

                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            tracing::warn!("Max connections ({}) reached, rejecting {}", config.security.max_connections, peer_addr);
                            drop(stream);
                            continue;
                        }
                    };

                    tracing::debug!("New connection from {}", peer_addr);

                    let cache = self.cache.clone();
                    let config = config.clone();
                    let client = shared_client.clone();

                    tokio::spawn(async move {
                        let _permit = permit; // held until task completes

                        let io = TokioIo::new(stream);
                        let handler = ProxyHandler::new(cache, config, client, peer_addr);

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
                _ = &mut shutdown => {
                    tracing::info!("Shutting down gracefully");
                    break;
                }
            }
        }

        Ok(())
    }
}
