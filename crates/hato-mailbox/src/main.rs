//! `hato-mailbox` — the rendezvous server binary.
//!
//! Binds `127.0.0.1:$PORT` (default 8080) and serves the WebSocket mailbox at
//! `/v1/ws`. Stateless: no database, no disk. Stop with Ctrl+C.

use std::net::SocketAddr;

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .map(|p| p.parse())
        .transpose()
        .context("PORT must be a number")?
        .unwrap_or(8080);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!("hato-mailbox listening on ws://{addr}/v1/ws");
    hato_mailbox::serve(listener).await
}
