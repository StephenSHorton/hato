//! `hato` — a carrier pigeon for your files. 🐦
//!
//! One-shot: `code` / `get`, `send` / `receive`.
//! Contacts: `pair`, `listen`, `send --to`, `contacts`, `me`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hato_core::contacts::ContactBook;
use hato_core::identity;
use hato_core::offer::{self, OfferMsg, CONTACT_ALPN};
use hato_core::{BlobTicket, EndpointId, SecretKey};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

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
    /// Send a file or folder.
    ///
    /// Without `--to`, prints a full ticket (or use `code` for a short code).
    /// With `--to <contact>`, offers the file to a paired contact (they must be
    /// running `hato listen`).
    Send {
        /// The file or folder to send.
        path: PathBuf,
        /// Paired contact id or name (requires they run `hato listen`).
        #[arg(long = "to")]
        to: Option<String>,
        /// Force a relay-only ticket (strip direct addresses).
        #[arg(long)]
        relay: bool,
    },
    /// Receive using a ticket; saves into DIR (default: `.`).
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
    /// Pair with another machine (host a short code).
    Pair {
        /// Your display name (default: config / hostname).
        #[arg(long)]
        name: Option<String>,
        /// Number of secret words in the pair code.
        #[arg(long, default_value_t = hato_code::DEFAULT_WORDS)]
        words: usize,
        /// Rendezvous mailbox URL.
        #[arg(long)]
        mailbox: Option<String>,
        /// Allow plaintext `ws://` to a non-local mailbox.
        #[arg(long)]
        insecure_mailbox: bool,
        #[command(subcommand)]
        action: Option<PairAction>,
    },
    /// Stay online and accept file offers from paired contacts.
    Listen {
        /// Where to save received files (default: `.`).
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Auto-accept offers from contacts (skip confirmation).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show or update this machine's identity.
    Me {
        /// Set the local display name.
        #[arg(long = "set-name")]
        set_name: Option<String>,
    },
    /// Manage the contact book.
    Contacts {
        #[command(subcommand)]
        action: ContactsCmd,
    },
}

#[derive(Subcommand)]
enum PairAction {
    /// Join someone else's pair code.
    Join {
        /// The code printed by `hato pair` (e.g. `7-arcade-otter`).
        code: String,
        /// Your display name (default: config / hostname).
        #[arg(long)]
        name: Option<String>,
        /// Rendezvous mailbox URL.
        #[arg(long)]
        mailbox: Option<String>,
        /// Allow plaintext `ws://` to a non-local mailbox.
        #[arg(long)]
        insecure_mailbox: bool,
    },
}

#[derive(Subcommand)]
enum ContactsCmd {
    /// List paired contacts.
    List,
    /// Rename a contact's display name.
    Rename {
        /// Contact id or name.
        contact: String,
        /// New display name.
        new_name: String,
    },
    /// Remove a contact.
    Remove {
        /// Contact id or name.
        contact: String,
    },
}

/// JSON exchanged during pairing (over the SPAKE2-sealed channel).
#[derive(Debug, Serialize, Deserialize)]
struct PairPayload {
    v: u32,
    kind: String,
    display_name: String,
    endpoint_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint_addr: Option<String>,
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
        Cmd::Send { path, to, relay } => match to {
            Some(contact) => send_to(path, contact, relay).await,
            None => send(path, relay).await,
        },
        Cmd::Receive {
            ticket,
            dir,
            store_dir,
        } => receive(ticket, dir, store_dir).await,
        Cmd::Pair {
            name,
            words,
            mailbox,
            insecure_mailbox,
            action,
        } => match action {
            None => pair_host(name, words, resolve_mailbox(mailbox), insecure_mailbox).await,
            Some(PairAction::Join {
                code,
                name: join_name,
                mailbox: join_mailbox,
                insecure_mailbox: join_insecure,
            }) => {
                // Prefer join-level flags; fall back to outer pair flags.
                pair_join(
                    code,
                    join_name.or(name),
                    resolve_mailbox(join_mailbox.or(mailbox)),
                    join_insecure || insecure_mailbox,
                )
                .await
            }
        },
        Cmd::Listen { dir, yes } => listen(dir, yes).await,
        Cmd::Me { set_name } => me(set_name),
        Cmd::Contacts { action } => contacts_cmd(action),
    }
}

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

fn secret_key() -> Result<SecretKey> {
    identity::load_or_create_secret_key()
}

fn display_name_or(override_name: Option<String>) -> Result<String> {
    if let Some(n) = override_name {
        let n = n.trim().to_string();
        if n.is_empty() {
            bail!("name must not be empty");
        }
        // Persist so later offers use the same name.
        identity::set_display_name(&n)?;
        return Ok(n);
    }
    Ok(identity::load_or_create_config()?.display_name)
}

fn me(set_name: Option<String>) -> Result<()> {
    if let Some(name) = set_name {
        let cfg = identity::set_display_name(name)?;
        println!("✅  display name set to {:?}", cfg.display_name);
    }
    let sk = secret_key()?;
    let cfg = identity::load_or_create_config()?;
    let id = sk.public();
    println!("🐦  you are:");
    println!("    name:         {}", cfg.display_name);
    println!("    endpoint id:  {id}");
    println!("    short:        {}", id.fmt_short());
    if let Ok(dir) = identity::config_dir() {
        println!("    config dir:   {}", dir.display());
    }
    Ok(())
}

fn contacts_cmd(action: ContactsCmd) -> Result<()> {
    match action {
        ContactsCmd::List => {
            let book = ContactBook::load()?;
            if book.contacts.is_empty() {
                println!("(no contacts yet — run `hato pair` with a friend)");
                return Ok(());
            }
            println!("{:<16} {:<24} {:<12} LAST SEEN", "ID", "NAME", "ENDPOINT");
            for c in &book.contacts {
                let short = c
                    .endpoint_id()
                    .map(|e| e.fmt_short().to_string())
                    .unwrap_or_else(|_| c.endpoint_id.chars().take(10).collect());
                let seen = c
                    .last_seen
                    .map(|t| format!("{t}"))
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<16} {:<24} {:<12} {}",
                    c.id,
                    truncate(&c.name, 24),
                    short,
                    seen
                );
            }
            Ok(())
        }
        ContactsCmd::Rename { contact, new_name } => {
            let mut book = ContactBook::load()?;
            let c = book.rename(&contact, &new_name)?;
            let id = c.id.clone();
            let name = c.name.clone();
            book.save()?;
            println!("✅  renamed {id} → {name:?}");
            Ok(())
        }
        ContactsCmd::Remove { contact } => {
            let mut book = ContactBook::load()?;
            let c = book.remove(&contact)?;
            book.save()?;
            println!("👋  removed contact {} ({})", c.id, c.name);
            Ok(())
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

async fn build_pair_payload(name: Option<String>) -> Result<PairPayload> {
    let display_name = display_name_or(name)?;
    let sk = secret_key()?;
    // Go online briefly so we can share a dialable address.
    let endpoint = hato_core::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(sk.clone())
        .alpns(vec![CONTACT_ALPN.to_vec()])
        .bind()
        .await
        .context("bind endpoint for pairing")?;
    let _ = tokio::time::timeout(Duration::from_secs(20), endpoint.online()).await;
    let addr = endpoint.addr();
    let payload = PairPayload {
        v: 1,
        kind: "pair".into(),
        display_name,
        endpoint_id: endpoint.id().to_string(),
        // EndpointAddr is serde-serializable (no Display in iroh 1.0.1).
        endpoint_addr: serde_json::to_string(&addr).ok(),
    };
    endpoint.close().await;
    Ok(payload)
}

async fn pair_host(
    name: Option<String>,
    words: usize,
    mailbox: String,
    insecure: bool,
) -> Result<()> {
    let my = build_pair_payload(name).await?;
    let my_bytes = serde_json::to_vec(&my)?;

    println!("🔗  pairing as {:?} …", my.display_name);

    let peer_bytes = hato_code::pair_host(
        &mailbox,
        words,
        &my_bytes,
        insecure,
        |code| {
            println!("🐦  tell your friend to run:\n");
            println!("        hato pair join {code}\n");
            println!("    (single-use code; keep this window open)");
        },
        |sas| {
            println!("\n🔑  verify aloud (optional): {sas}");
        },
    )
    .await
    .map_err(map_code_err)?;

    finish_pair(&peer_bytes)
}

async fn pair_join(
    code: String,
    name: Option<String>,
    mailbox: String,
    insecure: bool,
) -> Result<()> {
    let my = build_pair_payload(name).await?;
    let my_bytes = serde_json::to_vec(&my)?;

    println!("🔗  pairing as {:?} …", my.display_name);
    println!("🔓  joining code at {mailbox} …");

    let peer_bytes = hato_code::pair_join(&mailbox, &code, &my_bytes, insecure, |sas| {
        println!("🔑  verify aloud (optional): {sas}");
    })
    .await
    .map_err(map_code_err)?;

    finish_pair(&peer_bytes)
}

fn finish_pair(peer_bytes: &[u8]) -> Result<()> {
    let peer: PairPayload = serde_json::from_slice(peer_bytes).context("peer pair payload")?;
    if peer.kind != "pair" || peer.v != 1 {
        bail!("peer sent an unexpected pair payload");
    }
    let endpoint_id: EndpointId = peer
        .endpoint_id
        .parse()
        .map_err(|e| anyhow::anyhow!("peer endpoint id: {e}"))?;

    let mut book = ContactBook::load()?;
    let id = book.upsert_paired(&peer.display_name, endpoint_id, peer.endpoint_addr);
    book.save()?;

    println!("✅  paired with {:?} as contact `{id}`", peer.display_name);
    println!("    endpoint: {endpoint_id}");
    println!("    later: they run `hato listen`, you run `hato send --to {id} <path>`");
    Ok(())
}

fn map_code_err(e: hato_code::Error) -> anyhow::Error {
    match e {
        hato_code::Error::VerifierMismatch => anyhow::anyhow!(
            "wrong code (or a man-in-the-middle): the code did not match. \
             Double-check the words; nothing was saved."
        ),
        other => anyhow::anyhow!("{other}"),
    }
}

// ---------------------------------------------------------------------------
// Listen / send --to
// ---------------------------------------------------------------------------

async fn listen(dir: PathBuf, auto_yes: bool) -> Result<()> {
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create receive dir {}", dir.display()))?;
    let sk = secret_key()?;
    let cfg = identity::load_or_create_config()?;
    let book = ContactBook::load()?;

    let endpoint = offer::bind_listener(sk).await?;
    println!(
        "👂  listening as {:?} ({})",
        cfg.display_name,
        endpoint.id().fmt_short()
    );
    println!(
        "    saving into {}",
        dir.canonicalize().unwrap_or(dir.clone()).display()
    );
    if book.contacts.is_empty() {
        println!("    ⚠  contact book is empty — pair with someone first (`hato pair`)");
    } else {
        println!(
            "    {} contact(s); offers from unknowns are rejected",
            book.contacts.len()
        );
    }
    if auto_yes {
        println!("    auto-accept: on");
    }
    println!("    (Ctrl+C to stop)\n");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n👋  stopped listening.");
                endpoint.close().await;
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    bail!("endpoint closed");
                };
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("⚠  failed to accept connection: {e}");
                        continue;
                    }
                };
                let remote = conn.remote_id();
                let book = ContactBook::load()?;
                let contact_name = book
                    .by_endpoint(&remote)
                    .map(|c| format!("{} ({})", c.name, c.id))
                    .unwrap_or_else(|| remote.fmt_short().to_string());

                let store = listen_store_dir(&remote);
                let out = dir.clone();
                let yes = auto_yes;
                match offer::handle_offer_connection(
                    conn,
                    &out,
                    &store,
                    |id| book.contains_endpoint(id),
                    yes,
                    |_, offer| {
                        println!(
                            "📦  offer from {contact_name}: {:?} ({})",
                            offer.label,
                            offer
                                .bytes
                                .map(|b| HumanBytes(b).to_string())
                                .unwrap_or_else(|| "?".into())
                        );
                        if yes {
                            true
                        } else {
                            // Non-interactive default: accept from known contacts.
                            // (TTY prompt can come later; --yes documents the intent.)
                            println!("    accepting (known contact) …");
                            true
                        }
                    },
                    |_, _| {},
                )
                .await
                {
                    Ok(Some((remote, offer, summary))) => {
                        let mut book = ContactBook::load()?;
                        book.touch(&remote);
                        let _ = book.save();
                        println!(
                            "✅  received {} ({}) into {}",
                            offer.label,
                            HumanBytes(summary.total_bytes),
                            out.display()
                        );
                        let _ = std::fs::remove_dir_all(&store);
                    }
                    Ok(None) => {
                        println!("    declined.");
                    }
                    Err(e) => {
                        eprintln!("⚠  offer failed: {e:#}");
                        let _ = std::fs::remove_dir_all(&store);
                    }
                }
            }
        }
    }
    Ok(())
}

fn listen_store_dir(peer: &EndpointId) -> PathBuf {
    std::env::temp_dir().join("hato-listen").join(format!(
        "{}-{}",
        peer.fmt_short(),
        std::process::id()
    ))
}

async fn send_to(path: PathBuf, contact_query: String, relay_only: bool) -> Result<()> {
    if !path.exists() {
        bail!("no such file or folder: {}", path.display());
    }
    let book = ContactBook::load()?;
    let contact = book.resolve(&contact_query)?;
    let peer = contact.endpoint_id()?;
    let contact_id = contact.id.clone();
    let contact_name = contact.name.clone();

    let store = store_dir("send");
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("preparing {} …", path.display()));
    let outgoing = hato_core::prepare_send_identified(&path, &store, relay_only).await?;
    spinner.finish_and_clear();

    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let cfg = identity::load_or_create_config()?;
    let offer = OfferMsg::new(outgoing.ticket(), &label, &cfg.display_name);

    println!(
        "🐦  offering {:?} to {contact_name} ({contact_id}) …",
        label
    );
    println!("    (they need `hato listen` running)");

    let result = offer::send_offer(outgoing.endpoint(), peer, &offer).await;

    match result {
        Ok(()) => {
            println!("✅  {contact_name} finished downloading.");
            let mut book = ContactBook::load()?;
            book.touch(&peer);
            let _ = book.save();
        }
        Err(e) => {
            let _ = outgoing.shutdown().await;
            let _ = std::fs::remove_dir_all(&store);
            return Err(e);
        }
    }

    // Router/store shutdown can race with the peer closing the blob connection;
    // the file is already delivered at this point.
    let _ = outgoing.shutdown().await;
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}

// ---------------------------------------------------------------------------
// Classic one-shot paths
// ---------------------------------------------------------------------------

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
        bail!("no such file or folder: {}", path.display());
    }
    let store = store_dir("send");

    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("preparing {} …", path.display()));
    let sk = secret_key().ok();
    let outgoing = hato_core::prepare_send(&path, &store, relay_only, sk).await?;
    spinner.finish_and_clear();

    let ticket_bytes = hato_core::ticket_to_bytes(outgoing.ticket());

    if relay_only {
        println!("📡  relay-only ticket (no direct addresses — routes via iroh relay)\n");
    }

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
            bail!(
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
        bail!("no such file or folder: {}", path.display());
    }
    let store = store_dir("send");

    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("preparing {} …", path.display()));
    let sk = secret_key().ok();
    let outgoing = hato_core::prepare_send(&path, &store, relay_only, sk).await?;
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
    let sk = secret_key().ok();
    let summary = hato_core::receive_with_key(&ticket, &dir, &store, sk, move |done, total| {
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
