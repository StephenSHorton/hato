//! The spoken code format: `<nameplate>-<word>-<word>…` e.g. `7-arcade-otter`.
//!
//! The nameplate is a public routing handle (which mailbox slot to join) and is
//! NOT secret. The words carry all the entropy: each word encodes one random
//! byte via the PGP word list, alternating the even (two-syllable) and odd
//! (three-syllable) lists by position. Two words = 16 bits, which is safe *only*
//! because SPAKE2 turns an offline dictionary attack into a single online guess.

use rand::{rngs::OsRng, RngCore};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::wordlist::{EVEN, ODD};
use crate::{Error, Result};

/// A parsed code: a public nameplate plus the secret entropy bytes that seed the
/// PAKE. The secret is held in a [`Zeroizing`] buffer so it is wiped on drop.
pub struct ParsedCode {
    /// The mailbox nameplate (public routing handle).
    pub nameplate: u16,
    /// The decoded secret bytes — the SPAKE2 password. Wiped on drop.
    pub secret: Zeroizing<Vec<u8>>,
}

/// Draw `words` fresh secret bytes from the OS CSPRNG. One byte per word.
pub fn random_secret(words: usize) -> Zeroizing<Vec<u8>> {
    let mut v = vec![0u8; words];
    OsRng.fill_bytes(&mut v);
    Zeroizing::new(v)
}

/// The word for byte `b` at position `pos`, lower-cased. Even positions use the
/// two-syllable list, odd positions the three-syllable list.
fn word_at(pos: usize, b: u8) -> String {
    let table = if pos.is_multiple_of(2) { &EVEN } else { &ODD };
    table[b as usize].to_ascii_lowercase()
}

/// Look up the byte for `word` at position `pos` (case-insensitive). Returns
/// `None` if the word is not in the list expected at that position.
fn byte_at(pos: usize, word: &str) -> Option<u8> {
    let table = if pos.is_multiple_of(2) { &EVEN } else { &ODD };
    table
        .iter()
        .position(|w| w.eq_ignore_ascii_case(word))
        .map(|i| i as u8)
}

/// Render a full code string from a nameplate and secret bytes.
pub fn encode(nameplate: u16, secret: &[u8]) -> String {
    let mut out = nameplate.to_string();
    for (pos, &b) in secret.iter().enumerate() {
        out.push('-');
        out.push_str(&word_at(pos, b));
    }
    out
}

/// Just the SAS/word portion for `secret` (no nameplate), e.g. `arcade-otter`.
/// Used to render the short authentication string from a verifier prefix.
pub fn words_only(secret: &[u8]) -> String {
    secret
        .iter()
        .enumerate()
        .map(|(pos, &b)| word_at(pos, b))
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalize raw user input: NFKC, lower-case, and collapse every run of
/// non-alphanumeric characters into a single `-`, trimming leading/trailing `-`.
fn normalize(input: &str) -> String {
    let folded: String = input.nfkc().collect::<String>().to_lowercase();
    let mut out = String::with_capacity(folded.len());
    let mut prev_dash = true; // trims leading separators
    for ch in folded.chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Parse a user-supplied code into its nameplate and secret bytes.
///
/// Accepts sloppy input (extra spaces, mixed case, `_`/`.` separators) via
/// [`normalize`]. Fails loudly on a missing nameplate, a nameplate out of `u16`
/// range, an empty word list, or any word not present in the expected list.
pub fn parse(input: &str) -> Result<ParsedCode> {
    let normalized = normalize(input);
    let mut parts = normalized.split('-').filter(|s| !s.is_empty());

    let nameplate_str = parts.next().ok_or(Error::Code("code is empty"))?;
    let nameplate: u16 = nameplate_str
        .parse()
        .map_err(|_| Error::Code("code must start with a numeric nameplate"))?;

    let mut secret = Vec::new();
    for (pos, word) in parts.enumerate() {
        let b = byte_at(pos, word).ok_or(Error::Code("unrecognised word in code"))?;
        secret.push(b);
    }
    if secret.is_empty() {
        return Err(Error::Code("code has no words"));
    }

    Ok(ParsedCode {
        nameplate,
        secret: Zeroizing::new(secret),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip() {
        for words in 1..=6usize {
            for _ in 0..64 {
                let secret = random_secret(words);
                let nameplate: u16 = (rand::random::<u16>()) % 1000;
                let code = encode(nameplate, &secret);
                let parsed = parse(&code).unwrap();
                assert_eq!(parsed.nameplate, nameplate);
                assert_eq!(&*parsed.secret, &*secret, "roundtrip failed for {code}");
            }
        }
    }

    #[test]
    fn known_vector() {
        // byte 0 at even pos -> "aardvark"; byte 0 at odd pos -> "adroitness".
        assert_eq!(encode(7, &[0, 0]), "7-aardvark-adroitness");
        // byte 255 -> "zulu" (even, lower-cased) / "yucatan" (odd).
        assert_eq!(encode(1, &[255, 255]), "1-zulu-yucatan");
    }

    #[test]
    fn parse_is_forgiving() {
        // Mixed case, underscores, stray spaces all normalize to the same code.
        let a = parse("7-AARDVARK-Adroitness").unwrap();
        let b = parse("  7 _ aardvark . adroitness  ").unwrap();
        assert_eq!(a.nameplate, 7);
        assert_eq!(&*a.secret, &[0, 0]);
        assert_eq!(&*b.secret, &*a.secret);
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(parse("").is_err());
        assert!(parse("notanumber-aardvark").is_err());
        assert!(parse("7").is_err()); // no words
        assert!(parse("7-notaword-adroitness").is_err());
        // A three-syllable word in an even slot must be rejected (position matters).
        assert!(parse("7-adroitness").is_err());
    }
}
