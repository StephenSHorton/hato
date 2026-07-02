//! `hato` — a carrier pigeon for your files. 🐦
//!
//! `hato send <path>`     imports a file/folder and prints a ticket.
//! `hato receive <ticket>` downloads it into the current (or given) directory.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hato_core::BlobTicket;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

#[derive(Parser)]
#[command(name = "hato", version, about = "A carrier pigeon for your files 🐦")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a file or folder; prints a ticket for your friend.
    Send {
        /// The file or folder to send.
        path: PathBuf,
        /// Force a relay-only ticket (strip direct addresses) — routes the
        /// transfer through iroh's relay, as if the receiver were on another
        /// network. Useful for testing the real cross-internet path.
        #[arg(long)]
        relay: bool,
    },
    /// Receive using a ticket; saves into DIR (default: current directory).
    Receive {
        /// The ticket printed by `hato send`.
        ticket: BlobTicket,
        /// Where to save the received file(s).
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().cmd {
        Cmd::Send { path, relay } => send(path, relay).await,
        Cmd::Receive { ticket, dir } => receive(ticket, dir).await,
    }
}

/// A per-process scratch directory for the blob store.
fn store_dir(role: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hato-{role}-{}", std::process::id()))
}

async fn send(path: PathBuf, relay_only: bool) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("no such file or folder: {}", path.display());
    }
    let store = store_dir("send");

    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("preparing {} …", path.display()));
    let outgoing = hato_core::prepare_send(&path, &store, relay_only).await?;
    spinner.finish_and_clear();

    if relay_only {
        println!("📡  relay-only ticket (no direct addresses — routes via iroh relay)\n");
    }
    println!("🐦  ready — your friend runs:\n");
    println!("        hato receive {}\n", outgoing.ticket());
    println!("    (keep this window open; press Ctrl+C when you're done)");

    tokio::signal::ctrl_c().await?;
    println!("\n👋  done serving.");
    outgoing.shutdown().await?;
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}

async fn receive(ticket: BlobTicket, dir: PathBuf) -> Result<()> {
    let store = store_dir("recv");

    let (relays, ips) = hato_core::ticket_addr_summary(&ticket);
    println!("🔗  ticket offers {relays} relay(s), {ips} direct address(es)");
    if ips == 0 {
        println!("    → no direct address: this transfer must go through the relay.");
    }

    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} receiving  {bytes} ({bytes_per_sec})")
            .unwrap(),
    );

    let progress = pb.clone();
    let total = hato_core::receive(&ticket, &dir, &store, move |bytes| {
        progress.set_position(bytes);
    })
    .await?;

    pb.finish_and_clear();
    println!("✅  received {} into {}", HumanBytes(total), dir.display());
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}
