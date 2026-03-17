use squiddish::{Config, ProxyServer, Result};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "squiddish=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration from environment variables (terminates on invalid values)
    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    let server = ProxyServer::new(config).await?;
    server.run().await?;

    Ok(())
}
