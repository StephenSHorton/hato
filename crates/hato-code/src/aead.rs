//! Authenticated encryption for the ticket phase.
//!
//! XChaCha20-Poly1305 with a fresh 24-byte `OsRng` nonce, prepended to the
//! ciphertext. Only the ticket bytes are ever encrypted; the key comes from the
//! PAKE key schedule (`k_ticket`). An `open()` failure is a hard error — it means
//! the ciphertext was tampered with or the wrong key was derived.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};

use crate::{Error, Result};

/// Length of the XChaCha20-Poly1305 nonce (extended nonce), in bytes.
const NONCE_LEN: usize = 24;

/// Encrypt `plaintext` under `key`, returning `nonce || ciphertext||tag`.
///
/// The nonce is drawn fresh from the operating system CSPRNG for every call, so
/// nonce reuse under a fixed key cannot happen even if `seal` is called twice.
pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());

    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);

    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| Error::Aead("encryption failed"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a `nonce || ciphertext||tag` blob produced by [`seal`].
///
/// Returns [`Error::Aead`] on any authentication failure — a tampered ciphertext
/// or a mismatched key both land here, and callers MUST treat it as fatal.
pub fn open(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < NONCE_LEN {
        return Err(Error::Aead("ciphertext too short to contain a nonce"));
    }
    let (nonce, ciphertext) = data.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| Error::Aead("decryption/authentication failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aead_roundtrip() {
        let key = [7u8; 32];
        let msg = b"a blob ticket goes here";
        let sealed = seal(&key, msg).unwrap();
        // The nonce must be prepended, so the blob is strictly longer than the
        // plaintext + tag, and must not equal the plaintext.
        assert!(sealed.len() > msg.len() + NONCE_LEN - 1);
        let opened = open(&key, &sealed).unwrap();
        assert_eq!(opened, msg);
    }

    #[test]
    fn aead_tamper_fails() {
        let key = [9u8; 32];
        let msg = b"tamper me";
        let mut sealed = seal(&key, msg).unwrap();
        // Flip a bit in the ciphertext body (past the 24-byte nonce).
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open(&key, &sealed).is_err());

        // A wrong key must also fail (no key confusion).
        let wrong = [10u8; 32];
        let fresh = seal(&key, msg).unwrap();
        assert!(open(&wrong, &fresh).is_err());
    }

    #[test]
    fn aead_nonce_is_random() {
        let key = [1u8; 32];
        let a = seal(&key, b"x").unwrap();
        let b = seal(&key, b"x").unwrap();
        // Two seals of the same plaintext must differ (fresh nonce each time).
        assert_ne!(a, b);
    }
}
