# Phase 2 — Short human-readable codes (design)

> Status: **design approved, not yet implemented.** Replaces the ~300-char
> `BlobTicket` with a short spoken code like `7-arcade-otter`. `hato send` /
> `hato receive` (raw tickets) stay as the `--raw` fallback.

## Decision: SPAKE2 (PAKE) over a self-hosted WebSocket mailbox

A naive short-code→ticket table is stealable; encrypting the ticket at rest is
still **offline-brute-forceable** (a curious server can crack a low-entropy code
at leisure). The only design that makes a *short* code safe is a **PAKE**: with
SPAKE2, nothing decryptable is ever stored, the server stays zero-knowledge, and
a MITM gets exactly **one online guess** per single-use mailbox — so 2 words
(16 bits) is safe by construction.

We take magic-wormhole's **security model** but implement it DIY on RustCrypto's
`spake2` (MIT/Apache) — **not** the `magic-wormhole` crate (EUPL-1.2 copyleft
would contaminate hato's MIT binary). No Argon2 needed (there's no offline
target). Result: PAKE-grade 2-word codes, MIT-clean, ~1,180 LOC.

## Crate layout

```
crates/
  hato-core/     # UNCHANGED transport; + ticket_to_bytes()/ticket_from_bytes()
  hato-cli/      # + `code` and `get` subcommands (keep send/receive as --raw)
  hato-code/     # NEW lib — the whole short-code protocol, NO iroh dependency
    src/{lib,wordlist,code,pake,aead,mailbox}.rs
  hato-mailbox/  # NEW bin — the axum WebSocket rendezvous server (~320 LOC)
```

`hato-code` works in `&[u8]`/`Vec<u8>`, never a `BlobTicket`, so it stays pure
crypto + WebSocket. The CLI does the `BlobTicket ↔ bytes` glue via two new
`hato-core` helpers (the ticket already implements `Display`/`FromStr`).

## CLI UX

```
hato code <PATH> [--words N=2] [--mailbox <URL>] [--relay]
    Import PATH (like `send`), open a rendezvous, print a short code
    (e.g. "7-arcade-otter") + an optional SAS line ("verify aloud: canyon-marble").
    Keeps serving until Ctrl+C.
hato get <CODE> [DIR] [--mailbox <URL>]
    Claim the rendezvous, run SPAKE2 with CODE, decrypt the ticket, then hand it
    to the existing hato_core::receive(). Aborts loudly on wrong code / MITM.
```

`--mailbox` also reads env `HATO_MAILBOX`; default baked to `wss://…`.

## Wire protocol (WebSocket, `GET /v1/ws`)

JSON control frames, base64 bodies. Server state `Map<nameplate:u16, Slot>`;
each slot holds ≤2 sides, a per-phase buffer, a TTL. **Server never sees the
code, the key, or the file bytes.**

| Dir | Frame | Meaning |
|---|---|---|
| C→S | `{"op":"allocate"}` | ask for a free nameplate |
| S→C | `{"op":"nameplate","id":7}` | lowest free u16 |
| C→S | `{"op":"open","id":7,"side":"S"\|"R"}` | join slot as a side |
| S→C | `{"op":"crowded"}` + close | a 3rd side tried → abort |
| C→S | `{"op":"add","phase":"pake"\|"verify"\|"ticket","body":<b64>}` | post to peer |
| S→C | `{"op":"msg","phase":…,"body":<b64>}` | relay of peer's `add` |
| S→C | `{"op":"gone"}` | peer closed / TTL expired |
| C→S | `{"op":"close","mood":"happy"}` | release + delete slot (single-use) |

Server rules: body ≤ 2 KiB; slot TTL ≤ 300 s; nameplate single-use (a 3rd
`open` → `crowded`); `tower_governor` per-IP rate limit + global slot cap; no
DB, no disk.

## Handshake

1. Both `start_symmetric(code, id=AppID)`, `add(pake)`, `finish(peer)` → 32-B `K`.
2. Both derive `verifier = HKDF(K,"hato:verifier")`, `add(verify)`, compare the
   peer's verifier in **constant time** (`subtle`). Mismatch → abort (the
   one-guess gate).
3. Only after a match, **sender** `add(ticket, XChaCha20Poly1305(k_ticket, ticket_bytes))`.
   Receiver decrypts, `close(happy)`.

Key schedule (domain-separated, transcript-bound):
```
K          = spake2.finish(peer)                       // 32 B
root       = HKDF-Extract(salt="hato:pake:v1", ikm=K || sorted(msg_S,msg_R))
verifier   = HKDF-Expand(root, "hato:verifier", 32)
k_ticket   = HKDF-Expand(root, "hato:phase:ticket", 32)
nonce      = OsRng[24]  (prepended to ciphertext)
```
Optional **SAS**: map `verifier[0..2]` → 2 words, print `verify aloud: …` on
both ends to defeat an active MITM who *heard* the code.

## Code format

- **PGP Word List** (256 even + 256 odd words, phonetically distinct, **public
  domain** — do NOT copy wormhole's list). 8 bits/word, alternating by position.
- Format `<nameplate>-<word>-<word>…` e.g. `7-arcade-otter`. Nameplate is a
  public routing handle (mailbox uniqueness), **not** secret — so words needn't
  be unique and carry all the security.
- Default 2 words = 16 bits (safe *because* it's PAKE: one online guess). `--words N` → 8N bits.
- Normalize: lowercase, NFKC, collapse separators to `-`.

## Security checklist (a reviewer MUST verify)

1. `spake2::start_symmetric` with a fixed shared `Identity`=AppID both sides; `finish()` error is fatal.
2. Verifier compared **constant-time** (`subtle`); mismatch aborts *before* the ticket phase. Sender never sends the ticket before a matching verifier.
3. Reflection resistance: transcript binds both messages with fixed side ordering; a self-reflection test must abort.
4. AEAD XChaCha20-Poly1305, 24-B `OsRng` nonce, `open()` failure = hard abort.
5. HKDF domain separation: distinct `info` for verifier vs ticket key; versioned salt.
6. All secrets from `OsRng`/`getrandom`, never time-seeded.
7. Single-use + crowded: server rejects a 3rd `open`; clients treat `crowded`/`gone` as a security abort, not a retry-in-place.
8. Nothing offline-brute-forceable is persisted anywhere (the PAKE invariant).
9. Production mailbox is `wss://`; refuse plaintext `ws://` unless `--insecure-mailbox` (dev).
10. Delivered `BlobTicket` is itself a bearer capability while serving — sender stops on Ctrl+C/first completion (document, don't hide).
11. Full-code disclosure is single-shot + loud (first redeemer wins) — inherent to all short codes; state in `--help`.
12. `zeroize` K, subkeys, ticket plaintext on drop.
13. No custom primitives — only RustCrypto; fixed protocol constants.

## Dependencies

`hato-code`: `spake2 0.4`, `hkdf 0.12` + `sha2 0.10`, `chacha20poly1305 0.10`,
`subtle 2`, `zeroize 1`, `rand 0.8` + `getrandom 0.2`,
`tokio-tungstenite 0.24` (rustls), `futures-util`, `serde`/`serde_json`,
`data-encoding 2`, `thiserror`.

`hato-mailbox`: `axum 0.7` (ws), `tokio`, `tower`, `tower_governor 0.4`,
`dashmap 6` (or `moka 0.12` TTL), `serde`/`serde_json`, `tracing`(+subscriber);
`shuttle-axum`/`shuttle-runtime` only for the deploy target.

## Build order + one-machine e2e

0. **Scaffold** — add both crates to `[workspace].members`; add the two `hato-core` helpers.
1. **Crypto, no network** — `code.rs`/`pake.rs`/`aead.rs` + wordlist. Unit tests:
   `pake_agrees` (same code → identical verifier + k_ticket), `pake_wrong_code`
   (different verifiers), `aead_roundtrip`, `aead_tamper_fails`, `code_roundtrip`.
2. **Mailbox + client** — server + `mailbox.rs`. Integration test in one process:
   spawn axum on `127.0.0.1:0`, two tasks run allocate→open→pake→verify→ticket
   over loopback, receiver recovers the exact input bytes. Add crowded/TTL/rate tests.
3. **CLI wiring + full e2e** on one machine:
   ```
   cargo run -p hato-mailbox                       # ws://127.0.0.1:8080/v1/ws
   hato code  file.txt   --mailbox ws://127.0.0.1:8080/v1/ws   # → 7-arcade-otter
   hato get   7-arcade-otter out --mailbox ws://127.0.0.1:8080/v1/ws
   diff file.txt out/file.txt && echo E2E-OK
   ```
   Plus a negative test (`hato get 7-wrong-word` → non-zero, "wrong code").
4. **Harden + SAS** — constant-time verifier, `zeroize`, rate limits, caps, TTL sweep, SAS, `--words`.
5. **Deploy** — `cargo shuttle deploy` the mailbox; bake `wss://` default.
6. *(optional, later)* pkarr/DHT zero-infra fallback reusing the same SPAKE2 core.
