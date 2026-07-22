//! Hato desktop GUI — a Tauri v2 shell around [`hato_core`]. 🐦
//!
//! Two panes:
//! * **Send** — pick (or drag-drop) a file/folder; we import + serve it via
//!   [`hato_core::prepare_send`] and hand back the ticket. The live [`Outgoing`]
//!   is parked in managed state so it keeps serving until *Stop*.
//! * **Receive** — paste a ticket; [`hato_core::receive`] streams it to disk and
//!   we forward `(done, total)` progress to the webview as `receive-progress`
//!   events for a live bar.
//!
//! The frontend is plain HTML/CSS/JS (see `ui/`) talking over `window.__TAURI__`
//! — no bundler, no npm. Native file dialogs are driven from Rust via the dialog
//! plugin, so the frontend needs no plugin JS bindings.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use hato_core::{BlobTicket, Outgoing};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex;

/// An in-flight outgoing transfer plus the scratch store backing it, so *Stop*
/// can both shut the transfer down and delete its on-disk store.
struct ActiveSend {
    outgoing: Outgoing,
    store_dir: PathBuf,
}

/// Managed state: the single active send, if any.
#[derive(Default)]
struct SendState(Mutex<Option<ActiveSend>>);

/// What the frontend needs after `start_send` succeeds.
#[derive(Serialize)]
struct SendResult {
    ticket: String,
    name: String,
    relays: usize,
    ips: usize,
}

/// What the frontend needs after `start_receive` completes.
#[derive(Serialize)]
struct RecvResult {
    total_bytes: u64,
    already_had: u64,
    out_dir: String,
    relays: usize,
    ips: usize,
}

/// Streamed to the webview as a `receive-progress` event during a download.
#[derive(Clone, Serialize)]
struct Progress {
    done: u64,
    total: u64,
}

/// A per-send scratch dir for the sender's blob store, unique per invocation so
/// concurrent/sequential sends never collide on one store.
fn send_store_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hato-gui-send-{}-{nanos}", std::process::id()))
}

/// The receiver's store dir, keyed by the transfer hash so an interrupted
/// download resumes on a re-run with the same ticket (mirrors the CLI).
fn recv_store_dir(ticket: &BlobTicket) -> PathBuf {
    let key = ticket.hash().to_string();
    let key = &key[..key.len().min(16)];
    std::env::temp_dir().join("hato-recv").join(key)
}

/// Open a native "pick file" dialog; returns the chosen path or `None` (cancel).
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

/// Open a native "pick folder" dialog; returns the chosen path or `None`.
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

/// The user's Downloads directory (default receive destination), if resolvable.
#[tauri::command]
fn default_download_dir(app: AppHandle) -> Option<String> {
    app.path()
        .download_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Import `path` and start serving it, parking the live [`Outgoing`] in state so
/// it keeps serving until [`stop_send`]. Any previous send is stopped first.
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

    // Stop any prior send *before* loading a new store dir.
    stop_active(&state).await;

    let store_dir = send_store_dir();
    // Prefer the machine's persistent identity when available (contacts-ready).
    let sk = hato_core::identity::load_or_create_secret_key().ok();
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

/// Stop the active send (if any) and clean up its scratch store.
#[tauri::command]
async fn stop_send(state: State<'_, SendState>) -> Result<(), String> {
    stop_active(&state).await;
    Ok(())
}

/// Shut down and forget the current [`ActiveSend`], deleting its store dir.
async fn stop_active(state: &State<'_, SendState>) {
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

/// Receive `ticket` into `dir` (or Downloads), streaming `(done, total)` to the
/// webview as `receive-progress` events for a live bar.
#[tauri::command]
async fn start_receive(
    app: AppHandle,
    ticket: String,
    dir: Option<String>,
) -> Result<RecvResult, String> {
    let ticket: BlobTicket = ticket
        .trim()
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
    .map_err(|e| format!("{e:#}"))?;

    let _ = std::fs::remove_dir_all(&store_dir);

    Ok(RecvResult {
        total_bytes: summary.total_bytes,
        already_had: summary.already_had,
        out_dir: outdir.to_string_lossy().into_owned(),
        relays,
        ips,
    })
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(SendState::default())
        .invoke_handler(tauri::generate_handler![
            pick_file,
            pick_folder,
            default_download_dir,
            start_send,
            stop_send,
            start_receive,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Hato GUI");
}
