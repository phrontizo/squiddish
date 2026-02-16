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

    let config = Config::from_file("config.toml").unwrap_or_else(|_| {
        tracing::warn!("Failed to load config.toml, using defaults");
        Config::default()
    });

    let server = ProxyServer::new(config).await?;
    server.run().await?;

    Ok(())
}
