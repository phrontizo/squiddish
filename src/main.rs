mod cache;
mod config;
mod error;
mod proxy;
mod apt;

use anyhow::Result;
use config::Config;
use proxy::ProxyServer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

async fn get_external_ip() -> Result<String> {
    let response = reqwest::get("https://api.ipify.org").await?;
    let ip = response.text().await?;
    Ok(ip)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "squiddish=info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration from environment variables (terminates on invalid values)
    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    // Log external IP address
    if let Ok(ip) = get_external_ip().await {
        tracing::info!("External IP address: {}", ip);
    }

    let server = ProxyServer::new(config).await?;
    server.run().await?;

    Ok(())
}
