//! Contact offer protocol — deliver a [`BlobTicket`] to a paired peer without a code.
//!
//! ALPN: [`CONTACT_ALPN`]. Sender dials the listener, sends an offer frame with
//! the ticket; listener accepts/rejects, pulls via existing blob receive, then
//! signals `done`.

use std::path::Path;

use anyhow::{bail, Context};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    Endpoint, EndpointId, SecretKey,
};
use serde::{Deserialize, Serialize};

use crate::{receive, ticket_addr_summary, BlobTicket, RecvSummary};

/// Application protocol for contact offers.
pub const CONTACT_ALPN: &[u8] = b"hato/contact/1";

/// Maximum control-message size (ticket strings are a few hundred bytes).
const MAX_MSG: usize = 64 * 1024;

/// Sender → listener: please pull this ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferMsg {
    pub v: u32,
    pub kind: String,
    pub ticket: String,
    pub label: String,
    #[serde(default)]
    pub bytes: Option<u64>,
    pub from_name: String,
}

impl OfferMsg {
    pub fn new(
        ticket: &BlobTicket,
        label: impl Into<String>,
        from_name: impl Into<String>,
    ) -> Self {
        Self {
            v: 1,
            kind: "offer".into(),
            ticket: ticket.to_string(),
            label: label.into(),
            bytes: None,
            from_name: from_name.into(),
        }
    }
}

/// Listener → sender control replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyMsg {
    pub v: u32,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ReplyMsg {
    pub fn accept() -> Self {
        Self {
            v: 1,
            kind: "accept".into(),
            reason: None,
        }
    }

    pub fn reject(reason: impl Into<String>) -> Self {
        Self {
            v: 1,
            kind: "reject".into(),
            reason: Some(reason.into()),
        }
    }

    pub fn done() -> Self {
        Self {
            v: 1,
            kind: "done".into(),
            reason: None,
        }
    }

    pub fn is_accept(&self) -> bool {
        self.kind == "accept"
    }

    pub fn is_done(&self) -> bool {
        self.kind == "done"
    }

    pub fn is_reject(&self) -> bool {
        self.kind == "reject"
    }
}

/// Bind a listen-only endpoint for contact offers.
pub async fn bind_listener(secret_key: SecretKey) -> anyhow::Result<Endpoint> {
    let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret_key)
        .alpns(vec![CONTACT_ALPN.to_vec()])
        .bind()
        .await
        .context("bind contact listener")?;
    // Best-effort online so peers can discover us via relay / address lookup.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), endpoint.online()).await;
    Ok(endpoint)
}

/// Send an offer to `peer` and wait until they accept and finish (or reject).
pub async fn send_offer(
    endpoint: &Endpoint,
    peer: EndpointId,
    offer: &OfferMsg,
) -> anyhow::Result<()> {
    let conn = endpoint
        .connect(peer, CONTACT_ALPN)
        .await
        .with_context(|| {
            format!(
                "could not reach contact {}: are they running `hato listen`?",
                peer.fmt_short()
            )
        })?;

    let (mut send, mut recv) = conn.open_bi().await.context("open offer stream")?;
    write_msg(&mut send, offer).await?;
    // Half-close write side; keep reading replies on `recv`.
    send.finish().context("finish offer send")?;

    let reply: ReplyMsg = read_msg(&mut recv).await.context("read accept/reject")?;
    if reply.is_reject() {
        bail!(
            "contact rejected the transfer: {}",
            reply.reason.as_deref().unwrap_or("no reason")
        );
    }
    if !reply.is_accept() {
        bail!("unexpected reply from contact: kind={}", reply.kind);
    }

    // Wait for done. A clean EOF after accept almost always means the peer
    // finished (and dropped the connection before our done frame was read);
    // treat that as success when the download path already completed on their side.
    match read_msg::<ReplyMsg>(&mut recv).await {
        Ok(msg) if msg.is_done() => Ok(()),
        Ok(msg) => bail!("unexpected final message: kind={}", msg.kind),
        Err(_) => Ok(()),
    }
}

/// Handle one accepted contact connection: verify peer is a contact, exchange
/// offer, pull the file, signal done.
///
/// `is_contact` returns true if the remote endpoint id is in the local book.
/// `on_progress` is forwarded to [`receive`].
pub async fn handle_offer_connection(
    conn: Connection,
    outdir: &Path,
    store_dir: &Path,
    is_contact: impl Fn(&EndpointId) -> bool,
    auto_accept: bool,
    mut on_offer: impl FnMut(&EndpointId, &OfferMsg) -> bool,
    mut on_progress: impl FnMut(u64, u64),
) -> anyhow::Result<Option<(EndpointId, OfferMsg, RecvSummary)>> {
    let remote = conn.remote_id();
    if !is_contact(&remote) {
        // Politely reject unknown peers.
        if let Ok((mut send, _recv)) = conn.accept_bi().await {
            let _ = write_msg(&mut send, &ReplyMsg::reject("not in contact book")).await;
            let _ = send.finish();
        }
        bail!("rejected offer from unknown peer {}", remote.fmt_short());
    }

    let (mut send, mut recv) = conn.accept_bi().await.context("accept offer stream")?;
    let offer: OfferMsg = read_msg(&mut recv).await.context("read offer")?;
    if offer.kind != "offer" || offer.v != 1 {
        write_msg(&mut send, &ReplyMsg::reject("bad offer")).await?;
        let _ = send.finish();
        bail!("malformed offer from {}", remote.fmt_short());
    }

    let accept = auto_accept || on_offer(&remote, &offer);
    if !accept {
        write_msg(&mut send, &ReplyMsg::reject("declined")).await?;
        let _ = send.finish();
        return Ok(None);
    }

    write_msg(&mut send, &ReplyMsg::accept()).await?;

    let ticket: BlobTicket = offer
        .ticket
        .parse()
        .map_err(|e| anyhow::anyhow!("offer ticket invalid: {e}"))?;
    let (relays, ips) = ticket_addr_summary(&ticket);
    tracing::debug!(
        peer = %remote.fmt_short(),
        relays,
        ips,
        label = %offer.label,
        "receiving contact offer"
    );

    let summary = receive(&ticket, outdir, store_dir, &mut on_progress)
        .await
        .context("download offered file")?;

    write_msg(&mut send, &ReplyMsg::done()).await?;
    if let Err(e) = send.finish() {
        tracing::debug!(%e, "finish after done");
    }
    // Wait until the peer ACKs the FIN so `done` is not lost when we drop `conn`.
    let _ = send.stopped().await;
    // Brief grace so the sender can read before we tear down.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    Ok(Some((remote, offer, summary)))
}

async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(msg).context("serialize control message")?;
    if bytes.len() > MAX_MSG {
        bail!("control message too large ({} bytes)", bytes.len());
    }
    let len = (bytes.len() as u32).to_be_bytes();
    send.write_all(&len).await.context("write msg len")?;
    send.write_all(&bytes).await.context("write msg body")?;
    Ok(())
}

async fn read_msg<T: for<'de> Deserialize<'de>>(recv: &mut RecvStream) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .context("read msg len")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MSG {
        bail!("invalid control message length {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.context("read msg body")?;
    serde_json::from_slice(&buf).context("parse control message")
}
