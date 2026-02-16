mod cache;
mod config;
mod error;
mod proxy;
mod apt;

use anyhow::Result;
use config::Config;
use proxy::ProxyServer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "squiddish=info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Try to load from file first, then fall back to environment variables
    let config = Config::from_file("config.toml").unwrap_or_else(|_| {
        tracing::info!("No config.toml found, loading from environment variables");
        Config::from_env()
    });

    let server = ProxyServer::new(config).await?;
    server.run().await?;

    Ok(())
}
