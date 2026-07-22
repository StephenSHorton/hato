//! Short human-readable transfer codes for Hato.
//!
//! This crate turns an opaque, ~300-character `BlobTicket` (which the caller
//! passes in as raw bytes — this crate never sees iroh) into a spoken code like
//! `7-arcade-otter`, and delivers it end-to-end encrypted through a
//! zero-knowledge WebSocket mailbox using SPAKE2.
//!
//! Security model (see `docs/phase2-shortcodes.md`): a PAKE means nothing
//! decryptable is ever stored or transmitted in the clear, the server stays
//! zero-knowledge, and a man-in-the-middle gets exactly **one online guess** per
//! single-use mailbox — so a 2-word (16-bit) code is safe by construction.
//!
//! Public API: [`send_ticket`] (host a rendezvous, print a code, deliver bytes)
//! and [`recv_ticket`] (redeem a code, recover the exact bytes). Both work purely
//! in `&[u8]` / `Vec<u8>`.

pub mod aead;
pub mod code;
pub mod mailbox;
pub mod pake;
pub mod wordlist;

use mailbox::{Mailbox, Phase, Side};

/// Default number of secret words in a code. 2 words = 16 bits, safe under PAKE.
pub const DEFAULT_WORDS: usize = 2;

/// Errors surfaced by the short-code protocol.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The code string was malformed or contained an unknown word.
    #[error("bad code: {0}")]
    Code(&'static str),
    /// The SPAKE2 handshake or key schedule failed.
    #[error("handshake error: {0}")]
    Pake(&'static str),
    /// Authenticated decryption failed (tampering or a mismatched key).
    #[error("authenticated-encryption error: {0}")]
    Aead(&'static str),
    /// A transport-level problem talking to the mailbox.
    #[error("mailbox error: {0}")]
    Mailbox(String),
    /// A third party tried to join the rendezvous — abort, do not retry in place.
    #[error("rendezvous is crowded: a third party tried to join — aborting")]
    Crowded,
    /// The peer left before completing the exchange.
    #[error("peer left the rendezvous before completing")]
    Gone,
    /// The verifiers did not match: a wrong code or an active man-in-the-middle.
    #[error("wrong code or man-in-the-middle: the verifier did not match")]
    VerifierMismatch,
    /// Refusing to send secrets over plaintext `ws://` to a non-local host.
    #[error(
        "refusing to use plaintext ws:// for remote host {0}: use wss:// or set --insecure-mailbox"
    )]
    Insecure(String),
    /// The peer violated the wire protocol.
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    /// A control frame failed to (de)serialize.
    #[error("frame (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Reject plaintext `ws://` to a remote host unless `insecure` is set. `wss://`
/// and loopback `ws://` are always allowed (loopback is the dev default).
fn require_secure(mailbox_url: &str, insecure: bool) -> Result<()> {
    let parsed =
        url::Url::parse(mailbox_url).map_err(|_| Error::Mailbox("invalid mailbox URL".into()))?;
    match parsed.scheme() {
        "wss" => Ok(()),
        "ws" => {
            if insecure {
                return Ok(());
            }
            let host = parsed.host_str().unwrap_or_default();
            let is_local = host == "localhost"
                || host == "::1"
                || host.starts_with("127.")
                || host.ends_with(".localhost");
            if is_local {
                Ok(())
            } else {
                Err(Error::Insecure(host.to_string()))
            }
        }
        other => Err(Error::Mailbox(format!(
            "unsupported mailbox scheme {other:?} (want ws:// or wss://)"
        ))),
    }
}

/// Host a rendezvous and deliver `payload` (opaque ticket bytes) to whoever
/// redeems the code.
///
/// Flow: connect → allocate a nameplate → build the code → `on_code(code)` →
/// SPAKE2 → constant-time verifier check → `on_verified(sas)` → AEAD-seal and
/// send the ticket → wait for the receiver to confirm. The ticket is only sent
/// *after* the verifiers match (checklist item 2).
///
/// * `words` — number of secret words in the code (min 1; 2 is the safe default).
/// * `on_code` — invoked once, as soon as the code is known, so the CLI can print
///   it for the human to relay.
/// * `on_verified` — invoked once, after the verifier matches, with the SAS words
///   ("verify aloud") — before the ticket is sent.
pub async fn send_ticket(
    mailbox_url: &str,
    words: usize,
    payload: &[u8],
    insecure: bool,
    on_code: impl FnOnce(&str),
    on_verified: impl FnOnce(&str),
) -> Result<()> {
    require_secure(mailbox_url, insecure)?;

    let mut mb = Mailbox::connect(mailbox_url).await?;
    let nameplate = mb.allocate().await?;

    let secret = code::random_secret(words.max(1));
    let code_str = code::encode(nameplate, &secret);
    on_code(&code_str);

    mb.open(nameplate, Side::S).await?;

    let (handshake, my_msg) = pake::start(&secret);
    mb.add(Phase::Pake, &my_msg).await?;
    let peer_msg = mb.recv_phase(Phase::Pake).await?;
    let session = handshake.finish(&peer_msg)?;

    mb.add(Phase::Verify, session.verifier()).await?;
    let peer_verifier = mb.recv_phase(Phase::Verify).await?;
    if !session.verifier_matches(&peer_verifier) {
        let _ = mb.close("errory").await;
        return Err(Error::VerifierMismatch);
    }
    on_verified(session.sas());

    // Only now, with a matching verifier, is it safe to release the ticket.
    let sealed = session.seal_ticket(payload)?;
    mb.add(Phase::Ticket, &sealed).await?;

    mb.wait_for_peer_done().await;
    let _ = mb.close("happy").await;
    Ok(())
}

/// Redeem `code`, run the handshake, and recover the exact bytes the sender sealed.
///
/// Aborts loudly on a wrong code / MITM ([`Error::VerifierMismatch`]), a crowded
/// or abandoned rendezvous, or an AEAD failure. `on_verified` is invoked with the
/// SAS words after the verifier matches.
pub async fn recv_ticket(
    mailbox_url: &str,
    code: &str,
    insecure: bool,
    on_verified: impl FnOnce(&str),
) -> Result<Vec<u8>> {
    require_secure(mailbox_url, insecure)?;

    // Parse first: an unknown word is caught here, before any network I/O.
    let parsed = code::parse(code)?;

    let mut mb = Mailbox::connect(mailbox_url).await?;
    mb.open(parsed.nameplate, Side::R).await?;

    let (handshake, my_msg) = pake::start(&parsed.secret);
    mb.add(Phase::Pake, &my_msg).await?;
    let peer_msg = mb.recv_phase(Phase::Pake).await?;
    let session = handshake.finish(&peer_msg)?;

    mb.add(Phase::Verify, session.verifier()).await?;
    let peer_verifier = mb.recv_phase(Phase::Verify).await?;
    if !session.verifier_matches(&peer_verifier) {
        let _ = mb.close("errory").await;
        return Err(Error::VerifierMismatch);
    }
    on_verified(session.sas());

    let sealed = mb.recv_phase(Phase::Ticket).await?;
    let ticket_bytes = session.open_ticket(&sealed)?;
    let _ = mb.close("happy").await;
    Ok(ticket_bytes)
}

/// Host a mutual pairing rendezvous: print a short code, exchange sealed
/// identity payloads with the peer, return the peer's payload bytes.
///
/// After SPAKE2 + verifier match, **both** sides send a `pair` phase message
/// (unlike one-way [`send_ticket`]). Callers typically serialize JSON with
/// display name + endpoint id.
pub async fn pair_host(
    mailbox_url: &str,
    words: usize,
    my_payload: &[u8],
    insecure: bool,
    on_code: impl FnOnce(&str),
    on_verified: impl FnOnce(&str),
) -> Result<Vec<u8>> {
    require_secure(mailbox_url, insecure)?;

    let mut mb = Mailbox::connect(mailbox_url).await?;
    let nameplate = mb.allocate().await?;

    let secret = code::random_secret(words.max(1));
    let code_str = code::encode(nameplate, &secret);
    on_code(&code_str);

    mb.open(nameplate, Side::S).await?;
    exchange_pair(&mut mb, &secret, my_payload, on_verified).await
}

/// Join a pairing rendezvous with `code`, send `my_payload`, return peer payload.
pub async fn pair_join(
    mailbox_url: &str,
    code: &str,
    my_payload: &[u8],
    insecure: bool,
    on_verified: impl FnOnce(&str),
) -> Result<Vec<u8>> {
    require_secure(mailbox_url, insecure)?;

    let parsed = code::parse(code)?;
    let mut mb = Mailbox::connect(mailbox_url).await?;
    mb.open(parsed.nameplate, Side::R).await?;
    exchange_pair(&mut mb, &parsed.secret, my_payload, on_verified).await
}

/// Shared pair handshake after the mailbox slot is open.
async fn exchange_pair(
    mb: &mut Mailbox,
    secret: &[u8],
    my_payload: &[u8],
    on_verified: impl FnOnce(&str),
) -> Result<Vec<u8>> {
    let (handshake, my_msg) = pake::start(secret);
    mb.add(Phase::Pake, &my_msg).await?;
    let peer_msg = mb.recv_phase(Phase::Pake).await?;
    let session = handshake.finish(&peer_msg)?;

    mb.add(Phase::Verify, session.verifier()).await?;
    let peer_verifier = mb.recv_phase(Phase::Verify).await?;
    if !session.verifier_matches(&peer_verifier) {
        let _ = mb.close("errory").await;
        return Err(Error::VerifierMismatch);
    }
    on_verified(session.sas());

    let sealed_mine = session.seal_ticket(my_payload)?;
    mb.add(Phase::Pair, &sealed_mine).await?;
    let sealed_peer = mb.recv_phase(Phase::Pair).await?;
    let peer_bytes = session.open_ticket(&sealed_peer)?;

    let _ = mb.close("happy").await;
    Ok(peer_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_secure_policy() {
        // wss:// is always fine.
        assert!(require_secure("wss://mailbox.example.com/v1/ws", false).is_ok());
        // Loopback ws:// is fine for dev without the insecure flag.
        assert!(require_secure("ws://127.0.0.1:8080/v1/ws", false).is_ok());
        assert!(require_secure("ws://localhost:8080/v1/ws", false).is_ok());
        // Plaintext ws:// to a remote host is refused unless explicitly allowed.
        assert!(matches!(
            require_secure("ws://mailbox.example.com/v1/ws", false),
            Err(Error::Insecure(_))
        ));
        assert!(require_secure("ws://mailbox.example.com/v1/ws", true).is_ok());
        // Nonsense scheme / URL is rejected.
        assert!(require_secure("http://example.com", false).is_err());
        assert!(require_secure("not a url", false).is_err());
    }
}
