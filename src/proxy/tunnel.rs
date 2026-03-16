use crate::error::Result;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpStream;

/// Handle HTTPS CONNECT tunneling using a pre-established target connection.
/// Uses copy_bidirectional which correctly handles TCP half-close,
/// ensuring data in both directions is fully drained before closing.
pub async fn handle_connect_tunnel(
    upgraded: Upgraded,
    mut target_stream: TcpStream,
    peer_addr: SocketAddr,
    target_addr: String,
) -> Result<()> {
    tracing::info!(
        "Establishing CONNECT tunnel from {} to {}",
        peer_addr,
        target_addr
    );

    let mut client_stream = TokioIo::new(upgraded);

    match tokio::io::copy_bidirectional(&mut client_stream, &mut target_stream).await {
        Ok((from_client, from_target)) => {
            tracing::debug!(
                "Tunnel closed for {} (client->target: {} bytes, target->client: {} bytes)",
                target_addr, from_client, from_target
            );
        }
        Err(e) => {
            tracing::debug!("Tunnel error for {}: {}", target_addr, e);
        }
    }

    tracing::info!("CONNECT tunnel closed for {}", target_addr);
    Ok(())
}

