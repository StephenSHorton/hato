//! WebSocket client for the rendezvous mailbox.
//!
//! The mailbox is a dumb, zero-knowledge relay: it hands out nameplates and
//! forwards opaque base64 bodies between the (at most) two sides of a slot. It
//! never sees the code, the key, or the file. The wire frames below MUST stay in
//! lock-step with the server in `crates/hato-mailbox`.

use data_encoding::BASE64;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::{Error, Result};

/// Which side of a slot a client is joining.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    /// The sender (allocates the nameplate).
    S,
    /// The receiver (redeems the code).
    R,
}

/// Ordered phases of the handshake, used to tag relayed bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The SPAKE2 message exchange.
    Pake,
    /// The verifier exchange (constant-time compared).
    Verify,
    /// The AEAD-sealed ticket (sender → receiver only).
    Ticket,
    /// Bidirectional AEAD-sealed pair payloads (both sides send one).
    Pair,
}

/// Frames a client sends to the server.
#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ClientFrame<'a> {
    Allocate,
    Open { id: u16, side: Side },
    Add { phase: Phase, body: &'a str },
    Close { mood: &'a str },
}

/// Frames the server sends to a client.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ServerFrame {
    Nameplate { id: u16 },
    Crowded,
    Msg { phase: Phase, body: String },
    Gone,
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A connected mailbox session for one side of one rendezvous.
pub struct Mailbox {
    ws: Ws,
}

impl Mailbox {
    /// Open a WebSocket connection to `url`.
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(url)
            .await
            .map_err(|e| Error::Mailbox(format!("connect to {url} failed: {e}")))?;
        Ok(Self { ws })
    }

    async fn send_frame(&mut self, frame: &ClientFrame<'_>) -> Result<()> {
        let text = serde_json::to_string(frame)?;
        self.ws
            .send(Message::Text(text))
            .await
            .map_err(|e| Error::Mailbox(format!("send failed: {e}")))
    }

    async fn next_frame(&mut self) -> Result<ServerFrame> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str(&t).map_err(Error::from);
                }
                // Ignore control frames (tungstenite auto-replies to Ping).
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) | None => {
                    return Err(Error::Mailbox("connection closed by server".into()));
                }
                Some(Ok(other)) => {
                    return Err(Error::Mailbox(format!("unexpected frame: {other:?}")));
                }
                Some(Err(e)) => return Err(Error::Mailbox(format!("recv failed: {e}"))),
            }
        }
    }

    /// Ask the server for a fresh nameplate (sender only).
    pub async fn allocate(&mut self) -> Result<u16> {
        self.send_frame(&ClientFrame::Allocate).await?;
        match self.next_frame().await? {
            ServerFrame::Nameplate { id } => Ok(id),
            ServerFrame::Crowded => Err(Error::Crowded),
            _ => Err(Error::Protocol("expected a nameplate")),
        }
    }

    /// Join slot `id` as `side`. Success is silent; a rejection (`crowded`)
    /// surfaces at the next [`Mailbox::recv_phase`].
    pub async fn open(&mut self, id: u16, side: Side) -> Result<()> {
        self.send_frame(&ClientFrame::Open { id, side }).await
    }

    /// Post `body` to the peer under `phase`.
    pub async fn add(&mut self, phase: Phase, body: &[u8]) -> Result<()> {
        let encoded = BASE64.encode(body);
        self.send_frame(&ClientFrame::Add {
            phase,
            body: &encoded,
        })
        .await
    }

    /// Wait for the peer's message in `phase`, decoding its body.
    ///
    /// `crowded` and `gone` are surfaced as hard errors (a security abort, never
    /// a silent retry).
    pub async fn recv_phase(&mut self, phase: Phase) -> Result<Vec<u8>> {
        match self.next_frame().await? {
            ServerFrame::Msg { phase: got, body } if got == phase => BASE64
                .decode(body.as_bytes())
                .map_err(|_| Error::Protocol("peer body was not valid base64")),
            ServerFrame::Msg { .. } => Err(Error::Protocol("peer message arrived out of phase")),
            ServerFrame::Crowded => Err(Error::Crowded),
            ServerFrame::Gone => Err(Error::Gone),
            ServerFrame::Nameplate { .. } => Err(Error::Protocol("unexpected nameplate frame")),
        }
    }

    /// Wait (best-effort) for the peer to release the slot after a successful
    /// transfer. Any terminal signal — `gone`, a close, or a dropped socket — is
    /// treated as "peer is done".
    pub async fn wait_for_peer_done(&mut self) {
        match self.next_frame().await {
            Ok(ServerFrame::Gone) | Err(_) => {}
            Ok(_) => {}
        }
    }

    /// Release the slot and close the socket.
    pub async fn close(&mut self, mood: &str) -> Result<()> {
        let _ = self.send_frame(&ClientFrame::Close { mood }).await;
        let _ = self.ws.close(None).await;
        Ok(())
    }
}
