//! Hato desktop GUI — Tauri v2 shell around [`hato_core`] + contacts + auto-update.
//!
//! Panes:
//! * **Send** — ticket serve *or* offer to a paired contact
//! * **Receive** — paste a ticket
//! * **Contacts** — pair, list/rename/remove, listen for offers
//! * **Settings** — display name, mailbox, version / check for updates
//!
//! Plain HTML/CSS/JS frontend (`ui/`) via `window.__TAURI__`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod update;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hato_core::contacts::ContactBook;
use hato_core::identity;
use hato_core::offer::{self, OfferMsg, CONTACT_ALPN};
use hato_core::{BlobTicket, EndpointId, Outgoing, SecretKey};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{watch, Mutex};

const DEFAULT_MAILBOX: &str = "ws://127.0.0.1:8080/v1/ws";

// ── Shared state ──────────────────────────────────────────

struct ActiveSend {
    outgoing: Outgoing,
    store_dir: PathBuf,
}

#[derive(Default)]
struct SendState(Mutex<Option<ActiveSend>>);

struct ListenState {
    stop: Mutex<Option<watch::Sender<bool>>>,
    running: Arc<AtomicBool>,
}

impl Default for ListenState {
    fn default() -> Self {
        Self {
            stop: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

// ── DTOs ──────────────────────────────────────────────────

#[derive(Serialize)]
struct SendResult {
    ticket: String,
    name: String,
    relays: usize,
    ips: usize,
}

#[derive(Serialize)]
struct RecvResult {
    total_bytes: u64,
    already_had: u64,
    out_dir: String,
    relays: usize,
    ips: usize,
}

#[derive(Clone, Serialize)]
struct Progress {
    done: u64,
    total: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MeInfo {
    display_name: String,
    endpoint_id: String,
    endpoint_short: String,
    config_dir: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContactDto {
    id: String,
    name: String,
    endpoint_short: String,
    endpoint_id: String,
    last_seen: Option<u64>,
    paired_at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairResult {
    contact_id: String,
    contact_name: String,
    endpoint_id: String,
}

#[derive(Serialize, Deserialize)]
struct PairPayload {
    v: u32,
    kind: String,
    display_name: String,
    endpoint_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint_addr: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────

fn send_store_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hato-gui-send-{}-{nanos}", std::process::id()))
}

fn recv_store_dir(ticket: &BlobTicket) -> PathBuf {
    let key = ticket.hash().to_string();
    let key = &key[..key.len().min(16)];
    std::env::temp_dir().join("hato-recv").join(key)
}

fn listen_store_dir(peer: &EndpointId) -> PathBuf {
    std::env::temp_dir().join("hato-listen").join(format!(
        "{}-{}",
        peer.fmt_short(),
        std::process::id()
    ))
}

fn resolve_mailbox(flag: Option<String>) -> String {
    flag.filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HATO_MAILBOX").ok())
        .unwrap_or_else(|| DEFAULT_MAILBOX.to_string())
}

fn secret_key() -> Result<SecretKey, String> {
    identity::load_or_create_secret_key().map_err(|e| format!("{e:#}"))
}

fn map_code_err(e: hato_code::Error) -> String {
    match e {
        hato_code::Error::VerifierMismatch => {
            "wrong code (or a man-in-the-middle): the code did not match. Nothing was saved.".into()
        }
        other => other.to_string(),
    }
}

async fn build_pair_payload(name: Option<String>) -> Result<PairPayload, String> {
    let display_name = if let Some(n) = name {
        let n = n.trim().to_string();
        if n.is_empty() {
            return Err("name must not be empty".into());
        }
        identity::set_display_name(&n).map_err(|e| format!("{e:#}"))?;
        n
    } else {
        identity::load_or_create_config()
            .map_err(|e| format!("{e:#}"))?
            .display_name
    };
    let sk = secret_key()?;
    let endpoint = hato_core::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(sk)
        .alpns(vec![CONTACT_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| format!("bind endpoint for pairing: {e:#}"))?;
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

fn finish_pair(peer_bytes: &[u8]) -> Result<PairResult, String> {
    let peer: PairPayload =
        serde_json::from_slice(peer_bytes).map_err(|e| format!("peer pair payload: {e}"))?;
    if peer.kind != "pair" || peer.v != 1 {
        return Err("peer sent an unexpected pair payload".into());
    }
    let endpoint_id: EndpointId = peer
        .endpoint_id
        .parse()
        .map_err(|e| format!("peer endpoint id: {e}"))?;

    let mut book = ContactBook::load().map_err(|e| format!("{e:#}"))?;
    let id = book.upsert_paired(&peer.display_name, endpoint_id, peer.endpoint_addr);
    book.save().map_err(|e| format!("{e:#}"))?;

    Ok(PairResult {
        contact_id: id,
        contact_name: peer.display_name,
        endpoint_id: endpoint_id.to_string(),
    })
}

async fn stop_active(state: &SendState) {
    let active = state.0.lock().await.take();
    if let Some(ActiveSend {
        outgoing,
        store_dir,
    }) = active
    {
        let _ = outgoing.shutdown().await;
        let _ = std::fs::remove_dir_all(&store_dir);
    }
}

// ── File dialogs / ticket send-receive ────────────────────

#[tauri::command]
async fn pick_file(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_file(move |picked| {
        let _ = tx.send(picked);
    });
    rx.await
        .ok()
        .flatten()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    rx.await
        .ok()
        .flatten()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn default_download_dir(app: AppHandle) -> Option<String> {
    app.path()
        .download_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
async fn start_send(
    state: State<'_, SendState>,
    path: String,
    relay: bool,
) -> Result<SendResult, String> {
    let path = PathBuf::from(path);
    if !path.exists() {
        return Err(format!("no such file or folder: {}", path.display()));
    }
    stop_active(&state).await;

    let store_dir = send_store_dir();
    let sk = identity::load_or_create_secret_key().ok();
    let outgoing = hato_core::prepare_send(&path, &store_dir, relay, sk)
        .await
        .map_err(|e| format!("{e:#}"))?;

    let ticket = outgoing.ticket().to_string();
    let (relays, ips) = hato_core::ticket_addr_summary(outgoing.ticket());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    *state.0.lock().await = Some(ActiveSend {
        outgoing,
        store_dir,
    });

    Ok(SendResult {
        ticket,
        name,
        relays,
        ips,
    })
}

#[tauri::command]
async fn stop_send(state: State<'_, SendState>) -> Result<(), String> {
    stop_active(&state).await;
    Ok(())
}

/// Normalize a pasted ticket: strip all whitespace (textareas / chat apps often
/// inject newlines or spaces into long base32 strings).
fn sanitize_ticket(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Map low-level iroh/QUIC failures into something a human can act on.
fn friendly_transfer_err(e: anyhow::Error) -> String {
    let full = format!("{e:#}");
    let lower = full.to_ascii_lowercase();
    if lower.contains("stream reset") || lower.contains("connection reset") {
        return format!(
            "connection dropped mid-transfer (sender closed or network glitch).\n\
             Ask the sender to keep Hato open on the Serving screen and try again \
             — interrupted downloads resume automatically.\n\
             ({full})"
        );
    }
    if lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("failed to connect")
    {
        return format!(
            "couldn't reach the sender. Make sure they still have Hato open and \
             Serving, and that both sides are online.\n\
             ({full})"
        );
    }
    full
}

#[tauri::command]
async fn start_receive(
    app: AppHandle,
    ticket: String,
    dir: Option<String>,
) -> Result<RecvResult, String> {
    let ticket: BlobTicket = sanitize_ticket(&ticket)
        .parse()
        .map_err(|e| format!("that doesn't look like a valid ticket: {e}"))?;

    let outdir = match dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => app
            .path()
            .download_dir()
            .map_err(|e| format!("couldn't resolve a Downloads folder: {e}"))?,
    };

    let (relays, ips) = hato_core::ticket_addr_summary(&ticket);
    let store_dir = recv_store_dir(&ticket);

    let emitter = app.clone();
    let summary = hato_core::receive(&ticket, &outdir, &store_dir, move |done, total| {
        let _ = emitter.emit("receive-progress", Progress { done, total });
    })
    .await
    .map_err(friendly_transfer_err)?;

    let _ = std::fs::remove_dir_all(&store_dir);

    Ok(RecvResult {
        total_bytes: summary.total_bytes,
        already_had: summary.already_had,
        out_dir: outdir.to_string_lossy().into_owned(),
        relays,
        ips,
    })
}

// ── Identity / contacts ───────────────────────────────────

#[tauri::command]
fn get_me() -> Result<MeInfo, String> {
    let sk = secret_key()?;
    let cfg = identity::load_or_create_config().map_err(|e| format!("{e:#}"))?;
    let id = sk.public();
    let config_dir = identity::config_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(MeInfo {
        display_name: cfg.display_name,
        endpoint_id: id.to_string(),
        endpoint_short: id.fmt_short().to_string(),
        config_dir,
    })
}

#[tauri::command]
fn set_display_name(name: String) -> Result<MeInfo, String> {
    let n = name.trim().to_string();
    if n.is_empty() {
        return Err("name must not be empty".into());
    }
    identity::set_display_name(&n).map_err(|e| format!("{e:#}"))?;
    get_me()
}

#[tauri::command]
fn list_contacts() -> Result<Vec<ContactDto>, String> {
    let book = ContactBook::load().map_err(|e| format!("{e:#}"))?;
    Ok(book
        .contacts
        .iter()
        .map(|c| {
            let short = c
                .endpoint_id()
                .map(|e| e.fmt_short().to_string())
                .unwrap_or_else(|_| c.endpoint_id.chars().take(10).collect());
            ContactDto {
                id: c.id.clone(),
                name: c.name.clone(),
                endpoint_short: short,
                endpoint_id: c.endpoint_id.clone(),
                last_seen: c.last_seen,
                paired_at: c.paired_at,
            }
        })
        .collect())
}

#[tauri::command]
fn rename_contact(contact: String, new_name: String) -> Result<ContactDto, String> {
    let mut book = ContactBook::load().map_err(|e| format!("{e:#}"))?;
    let c = book
        .rename(&contact, &new_name)
        .map_err(|e| format!("{e:#}"))?
        .clone();
    book.save().map_err(|e| format!("{e:#}"))?;
    let short = c
        .endpoint_id()
        .map(|e| e.fmt_short().to_string())
        .unwrap_or_else(|_| c.endpoint_id.chars().take(10).collect());
    Ok(ContactDto {
        id: c.id,
        name: c.name,
        endpoint_short: short,
        endpoint_id: c.endpoint_id,
        last_seen: c.last_seen,
        paired_at: c.paired_at,
    })
}

#[tauri::command]
fn remove_contact(contact: String) -> Result<(), String> {
    let mut book = ContactBook::load().map_err(|e| format!("{e:#}"))?;
    book.remove(&contact).map_err(|e| format!("{e:#}"))?;
    book.save().map_err(|e| format!("{e:#}"))?;
    Ok(())
}

#[tauri::command]
fn get_mailbox() -> String {
    resolve_mailbox(None)
}

// ── Pairing ───────────────────────────────────────────────

#[tauri::command]
async fn pair_host(
    app: AppHandle,
    name: Option<String>,
    mailbox: Option<String>,
    insecure: Option<bool>,
) -> Result<PairResult, String> {
    let my = build_pair_payload(name).await?;
    let my_bytes = serde_json::to_vec(&my).map_err(|e| e.to_string())?;
    let mailbox = resolve_mailbox(mailbox);
    let insecure = insecure.unwrap_or(false);
    let app_code = app.clone();
    let app_sas = app.clone();

    let peer_bytes = hato_code::pair_host(
        &mailbox,
        2,
        &my_bytes,
        insecure,
        move |code| {
            let _ = app_code.emit("pair-code", code.to_string());
        },
        move |sas| {
            let _ = app_sas.emit("pair-sas", sas.to_string());
        },
    )
    .await
    .map_err(map_code_err)?;

    finish_pair(&peer_bytes)
}

#[tauri::command]
async fn pair_join(
    app: AppHandle,
    code: String,
    name: Option<String>,
    mailbox: Option<String>,
    insecure: Option<bool>,
) -> Result<PairResult, String> {
    let my = build_pair_payload(name).await?;
    let my_bytes = serde_json::to_vec(&my).map_err(|e| e.to_string())?;
    let mailbox = resolve_mailbox(mailbox);
    let insecure = insecure.unwrap_or(false);
    let app_sas = app.clone();

    let peer_bytes = hato_code::pair_join(&mailbox, code.trim(), &my_bytes, insecure, move |sas| {
        let _ = app_sas.emit("pair-sas", sas.to_string());
    })
    .await
    .map_err(map_code_err)?;

    finish_pair(&peer_bytes)
}

// ── Listen ────────────────────────────────────────────────

#[tauri::command]
fn is_listening(state: State<'_, ListenState>) -> bool {
    state.running.load(Ordering::SeqCst)
}

#[tauri::command]
async fn start_listen(
    app: AppHandle,
    state: State<'_, ListenState>,
    dir: Option<String>,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already listening".into());
    }

    let outdir = match dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => app.path().download_dir().map_err(|e| {
            state.running.store(false, Ordering::SeqCst);
            format!("couldn't resolve Downloads: {e}")
        })?,
    };
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        state.running.store(false, Ordering::SeqCst);
        return Err(format!("create dir: {e}"));
    }

    let sk = match secret_key() {
        Ok(k) => k,
        Err(e) => {
            state.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    let endpoint = match offer::bind_listener(sk).await {
        Ok(ep) => ep,
        Err(e) => {
            state.running.store(false, Ordering::SeqCst);
            return Err(format!("{e:#}"));
        }
    };

    let (stop_tx, mut stop_rx) = watch::channel(false);
    *state.stop.lock().await = Some(stop_tx);

    let app2 = app.clone();
    let running = state.running.clone();
    let _ = app.emit(
        "listen-status",
        serde_json::json!({
            "running": true,
            "dir": outdir.to_string_lossy(),
            "endpointShort": endpoint.id().fmt_short().to_string(),
        }),
    );

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                incoming = endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = app2.emit("listen-error", format!("accept failed: {e}"));
                            continue;
                        }
                    };
                    let remote = conn.remote_id();
                    let book = match ContactBook::load() {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = app2.emit("listen-error", format!("contact book: {e:#}"));
                            continue;
                        }
                    };
                    let contact_name = book
                        .by_endpoint(&remote)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| remote.fmt_short().to_string());

                    let store = listen_store_dir(&remote);
                    let out = outdir.clone();
                    let emitter = app2.clone();
                    let emitter2 = app2.clone();
                    let cname = contact_name.clone();

                    // auto_accept=false so on_offer always runs (and we emit UI events).
                    match offer::handle_offer_connection(
                        conn,
                        &out,
                        &store,
                        |id| book.contains_endpoint(id),
                        false,
                        move |_, offer| {
                            let _ = emitter.emit(
                                "listen-offer",
                                serde_json::json!({
                                    "from": cname,
                                    "label": offer.label,
                                    "bytes": offer.bytes,
                                }),
                            );
                            true
                        },
                        move |done, total| {
                            let _ = emitter2.emit("listen-progress", Progress { done, total });
                        },
                    )
                    .await
                    {
                        Ok(Some((remote, offer, summary))) => {
                            if let Ok(mut book) = ContactBook::load() {
                                book.touch(&remote);
                                let _ = book.save();
                            }
                            let _ = app2.emit(
                                "listen-done",
                                serde_json::json!({
                                    "label": offer.label,
                                    "totalBytes": summary.total_bytes,
                                    "outDir": out.to_string_lossy(),
                                }),
                            );
                            let _ = std::fs::remove_dir_all(&store);
                        }
                        Ok(None) => {
                            let _ = app2.emit("listen-error", "offer declined".to_string());
                        }
                        Err(e) => {
                            let _ = app2.emit("listen-error", format!("{e:#}"));
                            let _ = std::fs::remove_dir_all(&store);
                        }
                    }
                }
            }
        }
        endpoint.close().await;
        running.store(false, Ordering::SeqCst);
        let _ = app2.emit("listen-status", serde_json::json!({ "running": false }));
    });

    Ok(())
}

#[tauri::command]
async fn stop_listen(state: State<'_, ListenState>) -> Result<(), String> {
    if let Some(tx) = state.stop.lock().await.take() {
        let _ = tx.send(true);
    }
    state.running.store(false, Ordering::SeqCst);
    Ok(())
}

// ── Send to contact ───────────────────────────────────────

#[tauri::command]
async fn send_to_contact(
    app: AppHandle,
    path: String,
    contact: String,
    relay: bool,
) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.exists() {
        return Err(format!("no such file or folder: {}", path.display()));
    }

    let book = ContactBook::load().map_err(|e| format!("{e:#}"))?;
    let c = book.resolve(&contact).map_err(|e| format!("{e:#}"))?;
    let peer = c.endpoint_id().map_err(|e| format!("{e:#}"))?;
    let contact_name = c.name.clone();

    let store = send_store_dir();
    let _ = app.emit(
        "send-to-status",
        serde_json::json!({ "phase": "preparing", "contact": contact_name }),
    );

    let outgoing = hato_core::prepare_send_identified(&path, &store, relay)
        .await
        .map_err(|e| format!("{e:#}"))?;

    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let cfg = identity::load_or_create_config().map_err(|e| format!("{e:#}"))?;
    let offer = OfferMsg::new(outgoing.ticket(), &label, &cfg.display_name);

    let _ = app.emit(
        "send-to-status",
        serde_json::json!({
            "phase": "offering",
            "contact": contact_name,
            "label": label,
        }),
    );

    let result = offer::send_offer(outgoing.endpoint(), peer, &offer).await;

    match result {
        Ok(()) => {
            if let Ok(mut book) = ContactBook::load() {
                book.touch(&peer);
                let _ = book.save();
            }
            let _ = outgoing.shutdown().await;
            let _ = std::fs::remove_dir_all(&store);
            let _ = app.emit(
                "send-to-status",
                serde_json::json!({
                    "phase": "done",
                    "contact": contact_name,
                    "label": label,
                }),
            );
            Ok(())
        }
        Err(e) => {
            let _ = outgoing.shutdown().await;
            let _ = std::fs::remove_dir_all(&store);
            Err(format!("{e:#}"))
        }
    }
}

// ── main ──────────────────────────────────────────────────

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(SendState::default())
        .manage(ListenState::default())
        .setup(|app| {
            let update = Arc::new(update::UpdateState::new(app.handle().clone()));
            app.manage(update.clone());
            update::spawn_auto_update(update);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pick_file,
            pick_folder,
            default_download_dir,
            start_send,
            stop_send,
            start_receive,
            get_me,
            set_display_name,
            list_contacts,
            rename_contact,
            remove_contact,
            get_mailbox,
            pair_host,
            pair_join,
            is_listening,
            start_listen,
            stop_listen,
            send_to_contact,
            update::get_version,
            update::check_for_update,
            update::download_and_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Hato GUI");
}
