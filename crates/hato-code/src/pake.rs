//! SPAKE2 handshake + the domain-separated key schedule.
//!
//! Both sides run `start_symmetric(password = secret, identity = APP_ID)`,
//! exchange one message, and `finish()` into a shared 32-byte `K`. `K` plus the
//! (sorted) transcript is fed through HKDF-SHA256 to derive a public `verifier`
//! and a secret `k_ticket`. The verifier is compared in constant time; a
//! mismatch is the single online guess a MITM is allowed, and it aborts the
//! transfer *before* any ticket ciphertext is sent.

use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::{aead, code, Error, Result};

/// Fixed shared identity string — binds the handshake to this app + protocol
/// version. Both sides MUST use the same value.
pub const APP_ID: &[u8] = b"hato:pake:v1";

/// Versioned HKDF salt (the extract step).
const HKDF_SALT: &[u8] = b"hato:pake:v1";
/// HKDF `info` for the (public) verifier.
const INFO_VERIFIER: &[u8] = b"hato:verifier";
/// HKDF `info` for the (secret) ticket key.
const INFO_TICKET: &[u8] = b"hato:phase:ticket";

/// An in-flight handshake: our SPAKE2 state plus the message we put on the wire.
/// Consumed by [`Handshake::finish`].
pub struct Handshake {
    inner: Spake2<Ed25519Group>,
    /// Our outbound PAKE message; retained for reflection detection + transcript
    /// binding.
    msg: Vec<u8>,
}

/// A completed, verified-capable session. Holds the public verifier, the derived
/// SAS words, and (privately) the ticket key, which is wiped on drop.
pub struct Session {
    verifier: [u8; 32],
    k_ticket: Zeroizing<[u8; 32]>,
    sas: String,
}

/// Begin a symmetric SPAKE2 handshake for `secret`, returning our state and the
/// message to hand to the peer (post as the `pake` phase).
pub fn start(secret: &[u8]) -> (Handshake, Vec<u8>) {
    let (inner, msg) =
        Spake2::<Ed25519Group>::start_symmetric(&Password::new(secret), &Identity::new(APP_ID));
    (
        Handshake {
            inner,
            msg: msg.clone(),
        },
        msg,
    )
}

impl Handshake {
    /// Complete the handshake against the peer's PAKE message.
    ///
    /// Rejects a reflected message (peer echoing our own bytes) outright, then
    /// runs SPAKE2 `finish` and the HKDF key schedule. A `finish` error (bad
    /// point encoding, etc.) is fatal.
    pub fn finish(self, peer_msg: &[u8]) -> Result<Session> {
        // Reflection resistance: a self-reflection can never be a real peer.
        if peer_msg == self.msg.as_slice() {
            return Err(Error::Pake("reflected handshake message"));
        }

        let mut key = self
            .inner
            .finish(peer_msg)
            .map_err(|_| Error::Pake("SPAKE2 finish failed"))?;

        // Transcript binding with a fixed (sorted) ordering so both sides agree
        // regardless of who is sender/receiver.
        let (lo, hi) = if self.msg <= peer_msg.to_vec() {
            (self.msg.as_slice(), peer_msg)
        } else {
            (peer_msg, self.msg.as_slice())
        };

        let mut ikm = Vec::with_capacity(key.len() + lo.len() + hi.len());
        ikm.extend_from_slice(&key);
        ikm.extend_from_slice(lo);
        ikm.extend_from_slice(hi);
        key.zeroize();

        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), &ikm);
        ikm.zeroize();

        let mut verifier = [0u8; 32];
        hk.expand(INFO_VERIFIER, &mut verifier)
            .map_err(|_| Error::Pake("HKDF expand (verifier) failed"))?;

        let mut k_ticket = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_TICKET, &mut *k_ticket)
            .map_err(|_| Error::Pake("HKDF expand (ticket) failed"))?;

        // SAS: first two verifier bytes -> two PGP words, for out-of-band
        // "verify aloud" against an active MITM who overheard the code.
        let sas = code::words_only(&verifier[0..2]);

        Ok(Session {
            verifier,
            k_ticket,
            sas,
        })
    }
}

impl Session {
    /// Our verifier, to post as the `verify` phase.
    pub fn verifier(&self) -> &[u8; 32] {
        &self.verifier
    }

    /// The short authentication string (`word-word`) to read aloud on both ends.
    pub fn sas(&self) -> &str {
        &self.sas
    }

    /// Constant-time comparison of the peer's verifier against ours. Returns
    /// `true` only on an exact match; wrong code / MITM ⇒ `false`.
    pub fn verifier_matches(&self, peer: &[u8]) -> bool {
        // Length guard first (length isn't secret); then constant-time compare.
        peer.len() == self.verifier.len() && self.verifier.ct_eq(peer).into()
    }

    /// Encrypt the ticket bytes under `k_ticket` (sender side).
    pub fn seal_ticket(&self, ticket_bytes: &[u8]) -> Result<Vec<u8>> {
        aead::seal(&self.k_ticket, ticket_bytes)
    }

    /// Decrypt the ticket bytes under `k_ticket` (receiver side).
    pub fn open_ticket(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        aead::open(&self.k_ticket, ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a full two-party handshake for a given secret on each side, returning
    /// the two completed sessions.
    fn handshake(secret_s: &[u8], secret_r: &[u8]) -> (Result<Session>, Result<Session>) {
        let (hs_s, msg_s) = start(secret_s);
        let (hs_r, msg_r) = start(secret_r);
        (hs_s.finish(&msg_r), hs_r.finish(&msg_s))
    }

    #[test]
    fn pake_agrees() {
        let secret = [0x12, 0x34];
        let (s, r) = handshake(&secret, &secret);
        let (s, r) = (s.unwrap(), r.unwrap());
        // Same code -> identical verifier, identical SAS, and a ticket sealed by
        // one side opens on the other (identical k_ticket).
        assert_eq!(s.verifier(), r.verifier());
        assert_eq!(s.sas(), r.sas());
        assert!(s.verifier_matches(r.verifier()));
        let ct = s.seal_ticket(b"hello ticket").unwrap();
        assert_eq!(r.open_ticket(&ct).unwrap(), b"hello ticket");
    }

    #[test]
    fn pake_wrong_code() {
        let (s, r) = handshake(&[0x00, 0x00], &[0x00, 0x01]);
        let (s, r) = (s.unwrap(), r.unwrap());
        // Different codes -> different verifiers; the constant-time check fails,
        // so the ticket phase is never entered.
        assert_ne!(s.verifier(), r.verifier());
        assert!(!s.verifier_matches(r.verifier()));
        assert!(!r.verifier_matches(s.verifier()));
        // And even if a wrong-code receiver tried, k_ticket differs -> AEAD fails.
        let ct = s.seal_ticket(b"secret").unwrap();
        assert!(r.open_ticket(&ct).is_err());
    }

    #[test]
    fn pake_reflection_aborts() {
        // Feeding our own message back to us must abort (checklist item 3).
        let (hs, msg) = start(&[0xAA, 0xBB]);
        assert!(hs.finish(&msg).is_err());
    }
}
