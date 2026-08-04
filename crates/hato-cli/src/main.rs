//! `hato` — a carrier pigeon for your files. 🐦
//!
//! One-shot: `code` / `get`, `send` / `receive`.
//! Contacts: `pair`, `listen`, `send --to`, `contacts`, `me`.
//!
//! Machine mode (`--json` or `HATO_OUTPUT=json`): NDJSON events on stdout only.
//! See `json_out` for the event protocol (used by suzuri's transfer engine).

mod json_out;

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
use serde_json::json;

use json_out::{fail, from_anyhow, CodedError, ExitCode, ProgressEmitter};

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
#[command(
    name = "hato",
    version,
    about = "A carrier pigeon for your files 🐦 (also built as suzuri-transfer)"
)]
struct Cli {
    /// Machine-readable NDJSON on stdout (for host embedding). Also enabled when
    /// `HATO_OUTPUT=json`.
    #[arg(long, global = true)]
    json: bool,

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

fn main() {
    // tracing → stderr so JSON mode can own stdout.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let env_json = std::env::var("HATO_OUTPUT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    json_out::set_enabled(cli.json || env_json);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = fail(ExitCode::Generic, e);
            std::process::exit(ExitCode::Generic.as_i32());
        }
    };

    let result = rt.block_on(run(cli.cmd));
    match result {
        Ok(()) => std::process::exit(ExitCode::Ok.as_i32()),
        Err(e) => {
            // Ctrl+C after a successful send is the normal shutdown path (130).
            if !json_out::enabled() && e.code != ExitCode::Interrupted {
                eprintln!("error: {e:#}");
            }
            std::process::exit(e.code.as_i32());
        }
    }
}

async fn run(cmd: Cmd) -> std::result::Result<(), CodedError> {
    match cmd {
        Cmd::Code {
            path,
            words,
            mailbox,
            relay,
            insecure_mailbox,
        } => code(
            path,
            words,
            resolve_mailbox(mailbox),
            relay,
            insecure_mailbox,
        )
        .await
        .map_err(from_anyhow),
        Cmd::Get {
            code,
            dir,
            mailbox,
            insecure_mailbox,
        } => get(code, dir, resolve_mailbox(mailbox), insecure_mailbox).await,
        Cmd::Send { path, to, relay } => match to {
            Some(contact) => send_to(path, contact, relay).await.map_err(from_anyhow),
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
            None => pair_host(name, words, resolve_mailbox(mailbox), insecure_mailbox)
                .await
                .map_err(from_anyhow),
            Some(PairAction::Join {
                code,
                name: join_name,
                mailbox: join_mailbox,
                insecure_mailbox: join_insecure,
            }) => pair_join(
                code,
                join_name.or(name),
                resolve_mailbox(join_mailbox.or(mailbox)),
                join_insecure || insecure_mailbox,
            )
            .await
            .map_err(from_anyhow),
        },
        Cmd::Listen { dir, yes } => listen(dir, yes).await.map_err(from_anyhow),
        Cmd::Me { set_name } => me(set_name).map_err(from_anyhow),
        Cmd::Contacts { action } => contacts_cmd(action).map_err(from_anyhow),
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
        identity::set_display_name(&n)?;
        return Ok(n);
    }
    Ok(identity::load_or_create_config()?.display_name)
}

fn me(set_name: Option<String>) -> Result<()> {
    if let Some(name) = set_name {
        let cfg = identity::set_display_name(name)?;
        if json_out::enabled() {
            json_out::emit(
                "me",
                json!({
                    "display_name": cfg.display_name,
                    "updated": true,
                }),
            );
        } else {
            println!("✅  display name set to {:?}", cfg.display_name);
        }
    }
    let sk = secret_key()?;
    let cfg = identity::load_or_create_config()?;
    let id = sk.public();
    let config_dir = identity::config_dir().ok();
    if json_out::enabled() {
        json_out::emit(
            "me",
            json!({
                "display_name": cfg.display_name,
                "endpoint_id": id.to_string(),
                "endpoint_short": id.fmt_short().to_string(),
                "config_dir": config_dir.as_ref().map(|p| p.display().to_string()),
            }),
        );
    } else {
        println!("🐦  you are:");
        println!("    name:         {}", cfg.display_name);
        println!("    endpoint id:  {id}");
        println!("    short:        {}", id.fmt_short());
        if let Some(dir) = config_dir {
            println!("    config dir:   {}", dir.display());
        }
    }
    Ok(())
}

fn contacts_cmd(action: ContactsCmd) -> Result<()> {
    match action {
        ContactsCmd::List => {
            let book = ContactBook::load()?;
            if json_out::enabled() {
                let contacts: Vec<serde_json::Value> = book
                    .contacts
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "name": c.name,
                            "endpoint_id": c.endpoint_id,
                            "last_seen": c.last_seen.map(|t| t.to_string()),
                        })
                    })
                    .collect();
                json_out::emit("contacts", json!({ "contacts": contacts }));
                return Ok(());
            }
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
            if json_out::enabled() {
                json_out::emit("renamed", json!({ "id": id, "name": name }));
            } else {
                println!("✅  renamed {id} → {name:?}");
            }
            Ok(())
        }
        ContactsCmd::Remove { contact } => {
            let mut book = ContactBook::load()?;
            let c = book.remove(&contact)?;
            book.save()?;
            if json_out::enabled() {
                json_out::emit("removed", json!({ "id": c.id, "name": c.name }));
            } else {
                println!("👋  removed contact {} ({})", c.id, c.name);
            }
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

    if !json_out::enabled() {
        println!("🔗  pairing as {:?} …", my.display_name);
    }

    let peer_bytes = hato_code::pair_host(
        &mailbox,
        words,
        &my_bytes,
        insecure,
        |code| {
            if json_out::enabled() {
                json_out::emit("code", json!({ "code": code, "kind": "pair" }));
            } else {
                println!("🐦  tell your friend to run:\n");
                println!("        hato pair join {code}\n");
                println!("    (single-use code; keep this window open)");
            }
        },
        |sas| {
            if json_out::enabled() {
                json_out::emit("sas", json!({ "sas": sas }));
            } else {
                println!("\n🔑  verify aloud (optional): {sas}");
            }
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

    if !json_out::enabled() {
        println!("🔗  pairing as {:?} …", my.display_name);
        println!("🔓  joining code at {mailbox} …");
    }

    let peer_bytes = hato_code::pair_join(&mailbox, &code, &my_bytes, insecure, |sas| {
        if json_out::enabled() {
            json_out::emit("sas", json!({ "sas": sas }));
        } else {
            println!("🔑  verify aloud (optional): {sas}");
        }
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

    if json_out::enabled() {
        json_out::emit(
            "paired",
            json!({
                "contact_id": id,
                "name": peer.display_name,
                "endpoint_id": endpoint_id.to_string(),
            }),
        );
    } else {
        println!("✅  paired with {:?} as contact `{id}`", peer.display_name);
        println!("    endpoint: {endpoint_id}");
        println!("    later: they run `hato listen`, you run `hato send --to {id} <path>`");
    }
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
    if json_out::enabled() {
        json_out::emit(
            "listening",
            json!({
                "display_name": cfg.display_name,
                "endpoint_id": endpoint.id().to_string(),
                "endpoint_short": endpoint.id().fmt_short().to_string(),
                "dir": dir.display().to_string(),
                "contacts": book.contacts.len(),
                "auto_accept": auto_yes,
            }),
        );
    } else {
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
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                if json_out::enabled() {
                    json_out::emit("stopped", json!({ "reason": "interrupt" }));
                } else {
                    println!("\n👋  stopped listening.");
                }
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
                        if json_out::enabled() {
                            json_out::emit("error", json!({
                                "code": "accept",
                                "message": e.to_string(),
                            }));
                        } else {
                            eprintln!("⚠  failed to accept connection: {e}");
                        }
                        continue;
                    }
                };
                let remote = conn.remote_id();
                let book = ContactBook::load()?;
                let contact_name = book
                    .by_endpoint(&remote)
                    .map(|c| format!("{} ({})", c.name, c.id))
                    .unwrap_or_else(|| remote.fmt_short().to_string());
                let contact_id = book
                    .by_endpoint(&remote)
                    .map(|c| c.id.clone());

                let store = listen_store_dir(&remote);
                let out = dir.clone();
                let yes = auto_yes;
                let contact_name_offer = contact_name.clone();
                match offer::handle_offer_connection(
                    conn,
                    &out,
                    &store,
                    |id| book.contains_endpoint(id),
                    yes,
                    |_, offer| {
                        if json_out::enabled() {
                            json_out::emit("offer", json!({
                                "from": contact_name_offer,
                                "contact_id": contact_id,
                                "label": offer.label,
                                "bytes": offer.bytes,
                            }));
                        } else {
                            println!(
                                "📦  offer from {contact_name_offer}: {:?} ({})",
                                offer.label,
                                offer
                                    .bytes
                                    .map(|b| HumanBytes(b).to_string())
                                    .unwrap_or_else(|| "?".into())
                            );
                            if yes {
                                // fall through
                            } else {
                                println!("    accepting (known contact) …");
                            }
                        }
                        true
                    },
                    |_, _| {},
                )
                .await
                {
                    Ok(Some((remote, offer, summary))) => {
                        let mut book = ContactBook::load()?;
                        book.touch(&remote);
                        let _ = book.save();
                        if json_out::enabled() {
                            json_out::emit("done", json!({
                                "label": offer.label,
                                "total_bytes": summary.total_bytes,
                                "out_dir": out.display().to_string(),
                            }));
                        } else {
                            println!(
                                "✅  received {} ({}) into {}",
                                offer.label,
                                HumanBytes(summary.total_bytes),
                                out.display()
                            );
                        }
                        let _ = std::fs::remove_dir_all(&store);
                    }
                    Ok(None) => {
                        if json_out::enabled() {
                            json_out::emit("offer_rejected", json!({}));
                        } else {
                            println!("    declined.");
                        }
                    }
                    Err(e) => {
                        if json_out::enabled() {
                            json_out::emit("error", json!({
                                "code": "offer",
                                "message": format!("{e:#}"),
                            }));
                        } else {
                            eprintln!("⚠  offer failed: {e:#}");
                        }
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
    if !json_out::enabled() {
        let spinner = ProgressBar::new_spinner();
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner.set_message(format!("preparing {} …", path.display()));
        let outgoing = hato_core::prepare_send_identified(&path, &store, relay_only).await?;
        spinner.finish_and_clear();
        return send_to_finish(outgoing, path, contact_id, contact_name, peer, store).await;
    }

    let outgoing = hato_core::prepare_send_identified(&path, &store, relay_only).await?;
    send_to_finish(outgoing, path, contact_id, contact_name, peer, store).await
}

async fn send_to_finish(
    outgoing: hato_core::Outgoing,
    path: PathBuf,
    contact_id: String,
    contact_name: String,
    peer: EndpointId,
    store: PathBuf,
) -> Result<()> {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let cfg = identity::load_or_create_config()?;
    let offer = OfferMsg::new(outgoing.ticket(), &label, &cfg.display_name);

    if json_out::enabled() {
        json_out::emit(
            "offering",
            json!({
                "to": contact_id,
                "name": contact_name,
                "label": label,
                "ticket": outgoing.ticket().to_string(),
            }),
        );
    } else {
        println!(
            "🐦  offering {:?} to {contact_name} ({contact_id}) …",
            label
        );
        println!("    (they need `hato listen` running)");
    }

    let result = offer::send_offer(outgoing.endpoint(), peer, &offer).await;

    match result {
        Ok(()) => {
            let mut book = ContactBook::load()?;
            book.touch(&peer);
            let _ = book.save();
            if json_out::enabled() {
                json_out::emit(
                    "done",
                    json!({
                        "to": contact_id,
                        "label": label,
                    }),
                );
            } else {
                println!("✅  {contact_name} finished downloading.");
            }
        }
        Err(e) => {
            let _ = outgoing.shutdown().await;
            let _ = std::fs::remove_dir_all(&store);
            return Err(e);
        }
    }

    let _ = outgoing.shutdown().await;
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}

// ---------------------------------------------------------------------------
// Classic one-shot paths
// ---------------------------------------------------------------------------

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

    let outgoing = if json_out::enabled() {
        let sk = secret_key().ok();
        hato_core::prepare_send(&path, &store, relay_only, sk).await?
    } else {
        let spinner = ProgressBar::new_spinner();
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner.set_message(format!("preparing {} …", path.display()));
        let sk = secret_key().ok();
        let o = hato_core::prepare_send(&path, &store, relay_only, sk).await?;
        spinner.finish_and_clear();
        o
    };

    let ticket_bytes = hato_core::ticket_to_bytes(outgoing.ticket());
    let (relays, ips) = hato_core::ticket_addr_summary(outgoing.ticket());

    if relay_only && !json_out::enabled() {
        println!("📡  relay-only ticket (no direct addresses — routes via iroh relay)\n");
    }

    let result = hato_code::send_ticket(
        &mailbox,
        words,
        &ticket_bytes,
        insecure_mailbox,
        |code| {
            if json_out::enabled() {
                json_out::emit(
                    "code",
                    json!({
                        "code": code,
                        "kind": "transfer",
                        "ticket": outgoing.ticket().to_string(),
                        "relays": relays,
                        "ips": ips,
                        "relay_only": relay_only,
                    }),
                );
            } else {
                println!("🐦  ready — tell your friend to run:\n");
                println!("        hato get {code}\n");
                println!("    (single-use code; keep this window open until they're done)");
            }
        },
        |sas| {
            if json_out::enabled() {
                json_out::emit("sas", json!({ "sas": sas }));
            } else {
                println!("\n🔑  verify aloud (optional): {sas}");
            }
        },
    )
    .await;

    if let Err(e) = result {
        let _ = outgoing.shutdown().await;
        let _ = std::fs::remove_dir_all(&store);
        return Err(anyhow::anyhow!("{e}").context("the rendezvous did not complete"));
    }

    if json_out::enabled() {
        json_out::emit("serving", json!({ "reason": "code_redeemed" }));
    } else {
        println!("\n📦  code redeemed — now serving the file …");
        println!("    (press Ctrl+C when your friend has finished downloading)");
    }

    tokio::signal::ctrl_c().await?;
    if json_out::enabled() {
        json_out::emit("stopped", json!({ "reason": "interrupt" }));
    } else {
        println!("\n👋  done serving.");
    }
    outgoing.shutdown().await?;
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}

async fn get(
    code: String,
    dir: PathBuf,
    mailbox: String,
    insecure_mailbox: bool,
) -> std::result::Result<(), CodedError> {
    if !json_out::enabled() {
        println!("🔓  redeeming code at {mailbox} …");
    }
    let ticket_bytes = match hato_code::recv_ticket(&mailbox, &code, insecure_mailbox, |sas| {
        if json_out::enabled() {
            json_out::emit("sas", json!({ "sas": sas }));
        } else {
            println!("🔑  verify aloud (optional): {sas}");
        }
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(hato_code::Error::VerifierMismatch) => {
            return Err(fail(
                ExitCode::VerifierMismatch,
                anyhow::anyhow!(
                    "wrong code (or a man-in-the-middle): the code did not match. \
                     Double-check the words; nothing was transferred."
                ),
            ));
        }
        Err(e) => return Err(from_anyhow(anyhow::anyhow!("{e}"))),
    };

    let ticket = hato_core::ticket_from_bytes(&ticket_bytes).map_err(from_anyhow)?;
    if json_out::enabled() {
        json_out::emit("code_accepted", json!({ "ticket": ticket.to_string() }));
    } else {
        println!("✅  code accepted — starting download.");
    }
    receive(ticket, dir, None).await
}

fn store_dir(role: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hato-{role}-{}", std::process::id()))
}

fn recv_store_dir(ticket: &BlobTicket) -> PathBuf {
    let key = ticket.hash().to_string();
    let key = &key[..key.len().min(16)];
    std::env::temp_dir().join("hato-recv").join(key)
}

async fn send(path: PathBuf, relay_only: bool) -> std::result::Result<(), CodedError> {
    if !path.exists() {
        return Err(fail(
            ExitCode::Usage,
            anyhow::anyhow!("no such file or folder: {}", path.display()),
        ));
    }
    let store = store_dir("send");

    let outgoing = if json_out::enabled() {
        let sk = secret_key().ok();
        hato_core::prepare_send(&path, &store, relay_only, sk)
            .await
            .map_err(from_anyhow)?
    } else {
        let spinner = ProgressBar::new_spinner();
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner.set_message(format!("preparing {} …", path.display()));
        let sk = secret_key().ok();
        let o = hato_core::prepare_send(&path, &store, relay_only, sk)
            .await
            .map_err(from_anyhow)?;
        spinner.finish_and_clear();
        o
    };

    let (relays, ips) = hato_core::ticket_addr_summary(outgoing.ticket());
    let ticket = outgoing.ticket().to_string();

    if json_out::enabled() {
        json_out::emit(
            "ready",
            json!({
                "ticket": ticket,
                "relays": relays,
                "ips": ips,
                "relay_only": relay_only,
                "path": path.display().to_string(),
            }),
        );
    } else {
        if relay_only {
            println!("📡  relay-only ticket (no direct addresses — routes via iroh relay)\n");
        }
        println!("🐦  ready — your friend runs:\n");
        println!("        hato receive {}\n", outgoing.ticket());
        println!("    (keep this window open; press Ctrl+C when you're done)");
    }

    // Ctrl+C → clean shutdown. Exit 130 so hosts can distinguish interrupt.
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(e) => {
            let _ = outgoing.shutdown().await;
            let _ = std::fs::remove_dir_all(&store);
            return Err(fail(ExitCode::Generic, e));
        }
    }

    if json_out::enabled() {
        json_out::emit("stopped", json!({ "reason": "interrupt" }));
    } else {
        println!("\n👋  done serving.");
    }
    // Best-effort cleanup: iroh may already be tearing down on SIGINT.
    // Always report 130 so hosts treat Ctrl+C as the normal end of `send`.
    let _ = outgoing.shutdown().await;
    let _ = std::fs::remove_dir_all(&store);
    Err(CodedError::new(
        ExitCode::Interrupted,
        anyhow::anyhow!("interrupted"),
    ))
}

async fn receive(
    ticket: BlobTicket,
    dir: PathBuf,
    store_override: Option<PathBuf>,
) -> std::result::Result<(), CodedError> {
    let store = store_override.unwrap_or_else(|| recv_store_dir(&ticket));

    let (relays, ips) = hato_core::ticket_addr_summary(&ticket);
    if json_out::enabled() {
        json_out::emit(
            "receiving",
            json!({
                "relays": relays,
                "ips": ips,
                "dir": dir.display().to_string(),
            }),
        );
    } else {
        println!("🔗  ticket offers {relays} relay(s), {ips} direct address(es)");
        if ips == 0 {
            println!("    → no direct address: this transfer must go through the relay.");
        }
    }

    let sk = secret_key().ok();
    let summary = if json_out::enabled() {
        let emitter = ProgressEmitter::new();
        hato_core::receive_with_key(&ticket, &dir, &store, sk, move |done, total| {
            emitter.on_progress(done, total);
        })
        .await
        .map_err(from_anyhow)?
    } else {
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
        let summary = hato_core::receive_with_key(&ticket, &dir, &store, sk, move |done, total| {
            bar.set_length(total);
            bar.set_position(done);
        })
        .await
        .map_err(from_anyhow)?;
        pb.finish_and_clear();
        summary
    };

    if summary.already_had > 0 {
        if json_out::enabled() {
            json_out::emit("resumed", json!({ "already_had": summary.already_had }));
        } else {
            println!(
                "↻  resumed — {} were already downloaded",
                HumanBytes(summary.already_had)
            );
        }
    }

    if json_out::enabled() {
        json_out::emit(
            "done",
            json!({
                "total_bytes": summary.total_bytes,
                "already_had": summary.already_had,
                "out_dir": dir.display().to_string(),
            }),
        );
    } else {
        println!(
            "✅  received {} into {}",
            HumanBytes(summary.total_bytes),
            dir.display()
        );
    }
    let _ = std::fs::remove_dir_all(&store);
    Ok(())
}
