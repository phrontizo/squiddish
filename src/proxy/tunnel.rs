use crate::error::{ProxyError, Result};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Handle HTTPS CONNECT tunneling
pub async fn handle_connect_tunnel(
    upgraded: Upgraded,
    target_host: String,
    target_port: u16,
    peer_addr: SocketAddr,
) -> Result<()> {
    tracing::info!(
        "Establishing CONNECT tunnel from {} to {}:{}",
        peer_addr,
        target_host,
        target_port
    );

    // Resolve and connect to target
    let target_addr = format!("{}:{}", target_host, target_port);
    let target_stream = TcpStream::connect(&target_addr).await.map_err(|e| {
        ProxyError::Tunnel(format!("Failed to connect to {}: {}", target_addr, e))
    })?;

    tracing::debug!("Connected to target {}", target_addr);

    // Wrap upgraded connection for async IO
    let mut upgraded_io = TokioIo::new(upgraded);

    // Split both streams
    let (mut client_reader, mut client_writer) = tokio::io::split(upgraded_io);
    let (mut target_reader, mut target_writer) = target_stream.into_split();

    // Bidirectional copy with graceful shutdown
    let client_to_target = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match client_reader.read(&mut buf).await {
                Ok(0) => {
                    tracing::debug!("Client closed connection to {}", target_addr);
                    let _ = target_writer.shutdown().await;
                    return Ok::<_, std::io::Error>(());
                }
                Ok(n) => {
                    target_writer.write_all(&buf[..n]).await?;
                }
                Err(e) => {
                    tracing::debug!("Error reading from client: {}", e);
                    return Err(e);
                }
            }
        }
    };

    let target_to_client = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match target_reader.read(&mut buf).await {
                Ok(0) => {
                    tracing::debug!("Target closed connection from {}", target_addr);
                    let _ = client_writer.shutdown().await;
                    return Ok::<_, std::io::Error>(());
                }
                Ok(n) => {
                    client_writer.write_all(&buf[..n]).await?;
                }
                Err(e) => {
                    tracing::debug!("Error reading from target: {}", e);
                    return Err(e);
                }
            }
        }
    };

    // Run both directions concurrently
    tokio::select! {
        result = client_to_target => {
            if let Err(e) = result {
                tracing::debug!("Client to target tunnel error: {}", e);
            }
        }
        result = target_to_client => {
            if let Err(e) = result {
                tracing::debug!("Target to client tunnel error: {}", e);
            }
        }
    }

    tracing::info!("CONNECT tunnel closed for {}", target_addr);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tunnel_module() {
        // Basic module test
        assert!(true);
    }
}
