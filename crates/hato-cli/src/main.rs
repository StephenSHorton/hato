//! `hato` — a carrier pigeon for your files. 🐦
//!
//! `hato code <path>`     imports a file/folder and prints a short spoken code.
//! `hato get <code>`      redeems a code and downloads into a directory.
//! `hato send <path>`     (raw) imports a file/folder and prints a full ticket.
//! `hato receive <ticket>` (raw) downloads it into a directory.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hato_core::BlobTicket;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

/// Dev default mailbox. A `wss://` production default is a TODO — no rendezvous
/// server is deployed yet (see `docs/phase2-shortcodes.md`, step 5).
const DEFAULT_MAILBOX: &str = "ws://127.0.0.1:8080/v1/ws";

/// Resolve the mailbox URL: explicit `--mailbox`, else `$HATO_MAILBOX`, else the
/// local dev default.
fn resolve_mailbox(flag: Option<String>) -> String {
    flag.or_else(|| std::env::var("HATO_MAILBOX").ok())
        .unwrap_or_else(|| DEFAULT_MAILBOX.to_string())
}

#[derive(Parser)]
#[command(name = "hato", version, about = "A carrier pigeon for your files 🐦")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a file/folder; prints a short spoken code (e.g. `7-arcade-otter`).
    ///
    /// The code is single-use and single-shot: the first person to redeem it
    /// wins, so read it out once and keep this window open until they're done
    /// (Ctrl+C to stop). Anyone who learns the whole code can redeem it once.
    Code {
        /// The file or folder to send.
        path: PathBuf,
        /// Number of secret words in the code (more words = harder to guess).
        #[arg(long, default_value_t = hato_code::DEFAULT_WORDS)]
        words: usize,
        /// Rendezvous mailbox URL (else `$HATO_MAILBOX`, else the local default).
        #[arg(long)]
        mailbox: Option<String>,
        /// Force a relay-only ticket (strip direct addresses).
        #[arg(long)]
        relay: bool,
        /// Allow plaintext `ws://` to a non-local mailbox (dev only, unsafe).
        #[arg(long)]
        insecure_mailbox: bool,
    },
    /// Redeem a short code and download the file(s) into DIR (default: `.`).
    Get {
        /// The code printed by `hato code` (e.g. `7-arcade-otter`).
        code: String,
        /// Where to save the received file(s).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Rendezvous mailbox URL (else `$HATO_MAILBOX`, else the local default).
        #[arg(long)]
        mailbox: Option<String>,
        /// Allow plaintext `ws://` to a non-local mailbox (dev only, unsafe).
        #[arg(long)]
        insecure_mailbox: bool,
    },
    /// (raw fallback) Send a file or folder; prints a full ticket for your friend.
    Send {
        /// The file or folder to send.
        path: PathBuf,
        /// Force a relay-only ticket (strip direct addresses) — routes the
        /// transfer through iroh's relay, as if the receiver were on another
        /// network. Useful for testing the real cross-internet path.
        #[arg(long)]
        relay: bool,
    },
    /// (raw fallback) Receive using a ticket; saves into DIR (default: `.`).
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
        Cmd::Code {
            path,
            words,
            mailbox,
            relay,
            insecure_mailbox,
        } => {
            code(
                path,
                words,
                resolve_mailbox(mailbox),
                relay,
                insecure_mailbox,
            )
            .await
        }
        Cmd::Get {
            code,
            dir,
            mailbox,
            insecure_mailbox,
        } => get(code, dir, resolve_mailbox(mailbox), insecure_mailbox).await,
        Cmd::Send { path, relay } => send(path, relay).await,
        Cmd::Receive {
            ticket,
            dir,
            store_dir,
        } => receive(ticket, dir, store_dir).await,
    }
}

/// `hato code` — import the file, host a rendezvous, print a short code, and keep
/// serving until the transfer is done.
async fn code(
    path: PathBuf,
    words: usize,
    mailbox: String,
    relay_only: bool,
    insecure_mailbox: bool,
) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("no such file or folder: {}", path.display());
    }
    let store = store_dir("send");

    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("preparing {} …", path.display()));
    let outgoing = hato_core::prepare_send(&path, &store, relay_only).await?;
    spinner.finish_and_clear();

    // The ~300-char ticket becomes the opaque payload delivered over the mailbox.
    let ticket_bytes = hato_core::ticket_to_bytes(outgoing.ticket());

    if relay_only {
        println!("📡  relay-only ticket (no direct addresses — routes via iroh relay)\n");
    }

    // Deliver the ticket end-to-end encrypted; the callbacks print the code (as
    // soon as it's known) and the SAS (once the handshake verifies).
    let result = hato_code::send_ticket(
        &mailbox,
        words,
        &ticket_bytes,
        insecure_mailbox,
        |code| {
            println!("🐦  ready — tell your friend to run:\n");
            println!("        hato get {code}\n");
            println!("    (single-use code; keep this window open until they're done)");
        },
        |sas| {
            println!("\n🔑  verify aloud (optional): {sas}");
        },
    )
    .await;

    if let Err(e) = result {
        let _ = outgoing.shutdown().await;
        let _ = std::fs::remove_dir_all(&store);
        return Err(anyhow::anyhow!("{e}").context("the rendezvous did not complete"));
    }

    println!("\n📦  code redeemed — now serving the file …");
    println!("    (press Ctrl+C when your friend has finished downloading)");

    tokio::signal::ctrl_c().await?;
    println!("\n👋  done serving.");
    outgoing.shutdown().await?;
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}

/// `hato get` — redeem a code, decrypt the ticket, then download with it.
async fn get(code: String, dir: PathBuf, mailbox: String, insecure_mailbox: bool) -> Result<()> {
    println!("🔓  redeeming code at {mailbox} …");
    let ticket_bytes = match hato_code::recv_ticket(&mailbox, &code, insecure_mailbox, |sas| {
        println!("🔑  verify aloud (optional): {sas}");
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(hato_code::Error::VerifierMismatch) => {
            anyhow::bail!(
                "wrong code (or a man-in-the-middle): the code did not match. \
                 Double-check the words; nothing was transferred."
            );
        }
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };

    let ticket = hato_core::ticket_from_bytes(&ticket_bytes)?;
    println!("✅  code accepted — starting download.");
    receive(ticket, dir, None).await
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
