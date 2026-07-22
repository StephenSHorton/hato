//! The Hato rendezvous mailbox — a zero-knowledge WebSocket relay.
//!
//! Two sides find each other through a short-lived, single-use *nameplate* and
//! exchange opaque base64 bodies (SPAKE2 messages, verifiers, and one sealed
//! ticket). The server **never** sees the code, the derived key, or the file: it
//! only routes bytes between the (at most) two members of a slot, then throws the
//! slot away. No database, no disk.
//!
//! Wire protocol (`GET /v1/ws`, JSON text frames) — must match the client in
//! `crates/hato-code/src/mailbox.rs`:
//!
//! | Dir | Frame |
//! |-----|-------|
//! | C→S | `{"op":"allocate"}` → S→C `{"op":"nameplate","id":N}` |
//! | C→S | `{"op":"open","id":N,"side":"S"\|"R"}` |
//! | S→C | `{"op":"crowded"}` (a third side tried) |
//! | C→S | `{"op":"add","phase":…,"body":<b64>}` → relayed as S→C `{"op":"msg",…}` |
//! | S→C | `{"op":"gone"}` (peer closed / TTL expired) |
//! | C→S | `{"op":"close","mood":…}` (release + delete the slot) |

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::Response,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

/// Runtime limits. All caps are enforced server-side so a hostile client can
/// neither exhaust memory nor keep a slot forever.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum length of a single base64 `body` field (checklist: ≤ 2 KiB).
    pub max_body: usize,
    /// Maximum length of any inbound text frame (guards the JSON parser).
    pub max_frame: usize,
    /// Slot time-to-live; the sweeper deletes older slots (checklist: ≤ 300 s).
    pub slot_ttl: Duration,
    /// How often the TTL sweeper runs.
    pub sweep_interval: Duration,
    /// Global cap on concurrent slots (memory bound).
    pub max_slots: usize,
    /// Per-IP token-bucket refill (cells restored per this many milliseconds).
    pub rate_per_ms: u64,
    /// Per-IP burst allowance.
    pub rate_burst: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_body: 2 * 1024,
            max_frame: 8 * 1024,
            slot_ttl: Duration::from_secs(300),
            sweep_interval: Duration::from_secs(5),
            max_slots: 4096,
            // Generous for local/dev; still a real per-IP limiter.
            rate_per_ms: 20,
            rate_burst: 200,
        }
    }
}

/// One connected websocket, addressed by the channel that feeds its writer task.
struct Conn {
    tx: mpsc::UnboundedSender<Message>,
}

/// A rendezvous slot: up to two members plus a per-member queue for messages
/// that arrive before that member has joined.
struct Slot {
    conns: [Option<Conn>; 2],
    queued: [Vec<Message>; 2],
    created: Instant,
}

impl Slot {
    fn new() -> Self {
        Self {
            conns: [None, None],
            queued: [Vec::new(), Vec::new()],
            created: Instant::now(),
        }
    }

    fn is_empty(&self) -> bool {
        self.conns.iter().all(Option::is_none)
    }
}

/// Shared server state: the slot table plus limits. Cheaply cloneable (`Arc`).
#[derive(Clone)]
pub struct AppState {
    slots: Arc<Mutex<HashMap<u16, Slot>>>,
    config: Arc<Config>,
}

impl AppState {
    /// Build fresh state with the given limits.
    pub fn new(config: Config) -> Self {
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
        }
    }

    /// Periodically evict slots older than the TTL, signalling any members with a
    /// final `gone`. Bounds every slot's lifetime regardless of client behaviour.
    pub fn spawn_ttl_sweeper(&self) {
        let slots = Arc::clone(&self.slots);
        let ttl = self.config.slot_ttl;
        let interval = self.config.sweep_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                let mut map = slots.lock().unwrap();
                let expired: Vec<u16> = map
                    .iter()
                    .filter(|(_, s)| s.created.elapsed() > ttl)
                    .map(|(id, _)| *id)
                    .collect();
                for id in expired {
                    if let Some(slot) = map.remove(&id) {
                        for conn in slot.conns.into_iter().flatten() {
                            let _ = conn.tx.send(frame(&ServerFrame::Gone));
                        }
                    }
                }
            }
        });
    }
}

/// Build the axum router with the per-IP rate-limit layer applied.
pub fn router(state: AppState) -> Router {
    let governor = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_millis(state.config.rate_per_ms))
            .burst_size(state.config.rate_burst)
            .finish()
            .expect("valid governor config"),
    );
    Router::new()
        .route("/v1/ws", get(ws_route))
        .layer(GovernorLayer { config: governor })
        .with_state(state)
}

/// Serve the mailbox on an already-bound listener until the process ends,
/// using the default [`Config`].
pub async fn serve(listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    serve_with(listener, Config::default()).await
}

/// Serve the mailbox with an explicit [`Config`] (used by tests to shorten the
/// TTL / rate window).
///
/// Uses `ConnectInfo<SocketAddr>` so the rate limiter can key on the peer IP.
pub async fn serve_with(listener: tokio::net::TcpListener, config: Config) -> anyhow::Result<()> {
    let state = AppState::new(config);
    state.spawn_ttl_sweeper();
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Wire frames (must mirror the client)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Side {
    S,
    R,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Pake,
    Verify,
    Ticket,
    /// Bidirectional pair payloads (contacts).
    Pair,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ClientFrame {
    Allocate,
    Open {
        id: u16,
        side: Side,
    },
    Add {
        phase: Phase,
        body: String,
    },
    Close {
        #[allow(dead_code)]
        mood: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ServerFrame {
    Nameplate { id: u16 },
    Crowded,
    Msg { phase: Phase, body: String },
    Gone,
}

/// Serialize a server frame into a websocket text message.
fn frame(f: &ServerFrame) -> Message {
    Message::Text(serde_json::to_string(f).expect("server frame serializes"))
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

async fn ws_route(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, addr))
}

/// What this connection currently is within the slot table.
#[derive(Default)]
struct Membership {
    /// A nameplate this connection allocated but may not yet have opened.
    reserved: Option<u16>,
    /// The slot + side index this connection has joined.
    member: Option<(u16, usize)>,
}

async fn handle_socket(socket: WebSocket, state: AppState, addr: SocketAddr) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Writer task: everything destined for this client flows through `tx`.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut me = Membership::default();

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            // Ignore control + stray binary frames.
            _ => continue,
        };

        if text.len() > state.config.max_frame {
            tracing::debug!(%addr, len = text.len(), "oversized frame; closing");
            break;
        }

        let frame: ClientFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(%addr, error = %e, "bad frame; closing");
                break;
            }
        };

        match frame {
            ClientFrame::Allocate => {
                if !handle_allocate(&state, &tx, &mut me) {
                    break; // capacity exhausted → drop the connection
                }
            }
            ClientFrame::Open { id, side } => {
                if !handle_open(&state, &tx, &mut me, id, side) {
                    break; // crowded / unknown nameplate → server closed us
                }
            }
            ClientFrame::Add { phase, body } => {
                if body.len() > state.config.max_body {
                    tracing::debug!(%addr, "oversized body; closing");
                    break;
                }
                if !handle_add(&state, &me, phase, body) {
                    break; // add before open, or peer gone
                }
            }
            ClientFrame::Close { .. } => break,
        }
    }

    cleanup(&state, &me);

    // Close the writer's channel so it flushes any already-queued frames (e.g. a
    // final `crowded`/`gone`) before the socket drops — an abort here would reset
    // the connection and swallow that last frame.
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

/// Reserve the lowest free nameplate for this connection.
fn handle_allocate(
    state: &AppState,
    tx: &mpsc::UnboundedSender<Message>,
    me: &mut Membership,
) -> bool {
    let mut map = state.slots.lock().unwrap();
    if map.len() >= state.config.max_slots {
        return false;
    }
    let Some(id) = (1u16..=u16::MAX).find(|id| !map.contains_key(id)) else {
        return false;
    };
    map.insert(id, Slot::new());
    me.reserved = Some(id);
    let _ = tx.send(frame(&ServerFrame::Nameplate { id }));
    true
}

/// Join slot `id`. Returns `false` if the connection should be closed (unknown
/// nameplate, or a third side made it crowded).
fn handle_open(
    state: &AppState,
    tx: &mpsc::UnboundedSender<Message>,
    me: &mut Membership,
    id: u16,
    _side: Side,
) -> bool {
    let mut map = state.slots.lock().unwrap();
    let Some(slot) = map.get_mut(&id) else {
        // Unknown or expired nameplate: tell the client the peer is gone.
        let _ = tx.send(frame(&ServerFrame::Gone));
        return false;
    };

    let Some(idx) = slot.conns.iter().position(Option::is_none) else {
        // Both sides already taken → the classic single-use "crowded" abort.
        let _ = tx.send(frame(&ServerFrame::Crowded));
        return false;
    };

    slot.conns[idx] = Some(Conn { tx: tx.clone() });
    me.member = Some((id, idx));

    // Deliver anything the peer queued before we arrived.
    for queued in std::mem::take(&mut slot.queued[idx]) {
        let _ = tx.send(queued);
    }
    true
}

/// Relay an `add` to the other side (or queue it if the peer hasn't joined yet).
fn handle_add(state: &AppState, me: &Membership, phase: Phase, body: String) -> bool {
    let Some((id, idx)) = me.member else {
        return false; // `add` before `open`
    };
    let mut map = state.slots.lock().unwrap();
    let Some(slot) = map.get_mut(&id) else {
        return false;
    };
    let peer = 1 - idx;
    let msg = frame(&ServerFrame::Msg { phase, body });
    match &slot.conns[peer] {
        Some(conn) => {
            let _ = conn.tx.send(msg);
        }
        None => slot.queued[peer].push(msg),
    }
    true
}

/// On disconnect/close: delete the slot (single-use) and signal the peer `gone`.
fn cleanup(state: &AppState, me: &Membership) {
    let mut map = state.slots.lock().unwrap();

    if let Some((id, idx)) = me.member {
        if let Some(slot) = map.remove(&id) {
            let peer = 1 - idx;
            if let Some(conn) = &slot.conns[peer] {
                let _ = conn.tx.send(frame(&ServerFrame::Gone));
            }
        }
        return;
    }

    // Allocated but never opened: drop the empty reservation.
    if let Some(id) = me.reserved {
        if map.get(&id).is_some_and(Slot::is_empty) {
            map.remove(&id);
        }
    }
}
