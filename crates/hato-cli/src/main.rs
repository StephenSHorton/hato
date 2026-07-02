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
        /// Override the scratch store directory (advanced; used for testing
        /// resume). Defaults to a temp dir keyed by the transfer hash.
        #[arg(long, hide = true)]
        store_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().cmd {
        Cmd::Send { path, relay } => send(path, relay).await,
        Cmd::Receive {
            ticket,
            dir,
            store_dir,
        } => receive(ticket, dir, store_dir).await,
    }
}

/// A per-process scratch directory for the sender's blob store.
fn store_dir(role: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hato-{role}-{}", std::process::id()))
}

/// The receiver's store dir, keyed by the transfer's hash so an interrupted
/// download can be resumed by re-running with the same ticket.
fn recv_store_dir(ticket: &BlobTicket) -> PathBuf {
    let key = ticket.hash().to_string();
    let key = &key[..key.len().min(16)];
    std::env::temp_dir().join("hato-recv").join(key)
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

async fn receive(ticket: BlobTicket, dir: PathBuf, store_override: Option<PathBuf>) -> Result<()> {
    let store = store_override.unwrap_or_else(|| recv_store_dir(&ticket));

    let (relays, ips) = hato_core::ticket_addr_summary(&ticket);
    println!("🔗  ticket offers {relays} relay(s), {ips} direct address(es)");
    if ips == 0 {
        println!("    → no direct address: this transfer must go through the relay.");
    }

    let pb = ProgressBar::new(0);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:30.cyan/blue}] \
             {bytes}/{total_bytes}  {binary_bytes_per_sec}  ETA {eta}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let bar = pb.clone();
    let summary = hato_core::receive(&ticket, &dir, &store, move |done, total| {
        bar.set_length(total);
        bar.set_position(done);
    })
    .await?;

    pb.finish_and_clear();
    if summary.already_had > 0 {
        println!(
            "↻  resumed — {} were already downloaded",
            HumanBytes(summary.already_had)
        );
    }
    println!(
        "✅  received {} into {}",
        HumanBytes(summary.total_bytes),
        dir.display()
    );
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}
