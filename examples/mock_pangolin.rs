//! Minimal HTTP/1.1 stand-in for the pangolin `/api/v1/traefik-config` endpoint.
//!
//! Reads a fixture from `MOCK_PANGOLIN_FIXTURE`, serves it forever at
//! `MOCK_PANGOLIN_LISTEN` (default `127.0.0.1:18080`) with a stable ETag
//! derived from the body's sha256, and honors `If-None-Match` with 304.
//!
//! Used by the cluster integration test (`tests/cluster_integration.rs`) and
//! by anyone running the controller locally without a real pangolin server.

use std::env;
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let listen = env::var("MOCK_PANGOLIN_LISTEN").unwrap_or_else(|_| "127.0.0.1:18080".into());
    let fixture = env::var("MOCK_PANGOLIN_FIXTURE")
        .context("MOCK_PANGOLIN_FIXTURE must point at a JSON fixture")?;

    let body = std::fs::read(&fixture).with_context(|| format!("reading {fixture}"))?;
    let mut h = Sha256::new();
    h.update(&body);
    let etag = format!("\"{:x}\"", h.finalize());

    let body = Arc::new(body);
    let etag = Arc::new(etag);

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    eprintln!(
        "mock_pangolin listening on {listen}, serving {fixture} ({} bytes, etag={})",
        body.len(),
        etag
    );

    loop {
        let (mut sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        let body = body.clone();
        let etag = etag.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut received = Vec::<u8>::new();
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                received.extend_from_slice(&buf[..n]);
                if received.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&received);
            let if_none_match = request.lines().find_map(|line| {
                let (k, v) = line.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("if-none-match") {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            });

            let response = if if_none_match.as_deref() == Some(etag.as_str()) {
                format!(
                    "HTTP/1.1 304 Not Modified\r\n\
                     ETag: {etag}\r\n\
                     Content-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                )
                .into_bytes()
            } else {
                let mut r = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     ETag: {etag}\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                r.extend_from_slice(&body);
                r
            };

            if let Err(e) = sock.write_all(&response).await {
                eprintln!("write to {peer}: {e}");
            }
            let _ = sock.shutdown().await;
        });
    }
}
