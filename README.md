# Hato 🐦

[![CI](https://github.com/StephenSHorton/hato/actions/workflows/ci.yml/badge.svg)](https://github.com/StephenSHorton/hato/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Built on iroh](https://img.shields.io/badge/built%20on-iroh-8A2BE2.svg)](https://github.com/n0-computer/iroh)

**A carrier pigeon for your files.** Fast, free, peer-to-peer file transfer — send a file straight to a friend over the internet. No cloud, no account, no size cap.

Built in Rust on [iroh](https://github.com/n0-computer/iroh): QUIC transport, automatic NAT hole-punching, a free relay fallback, and end-to-end encryption where each side's key *is* its identity. The file streams **directly** between the two machines and **resumes** if the connection drops (BLAKE3 verified streaming), so multi-gigabyte sends are safe.

<p align="center"><img src="docs/hato-demo.svg" alt="hato send / receive demo" width="760"></p>

## Demo

**Sender** — point `hato` at a file (or folder); it prints a ticket and starts serving:

```console
$ hato send holiday-video.mkv
🐦  ready — your friend runs:

        hato receive blobabgv7c…q9m4qa

    (keep this window open; press Ctrl+C when you're done)
```

**Receiver** — paste the ticket; the file arrives with a live progress bar:

```console
$ hato receive blobabgv7c…q9m4qa
🔗  ticket offers 1 relay(s), 6 direct address(es)
 ⠹ [00:00:12] [==========================>---]  1.41 GiB/1.63 GiB  118 MiB/s  ETA 00:00:02
✅  received 1.63 GiB into .
```

If the transfer is interrupted, just run the same command again — it picks up where it left off:

```console
$ hato receive blobabgv7c…q9m4qa
↻  resumed — 1.41 GiB were already downloaded
✅  received 1.63 GiB into .
```

## Why hato

- **Direct & private** — bytes flow peer-to-peer, end-to-end encrypted. When the two machines can't reach each other directly, iroh falls back to a free relay automatically.
- **No size cap, safe to resume** — content is BLAKE3-verified as it streams, so an interrupted multi-GB send resumes from exactly where it stopped, even after a crash.
- **Zero setup** — no account, no server to run, no port forwarding.
- **Just a file transfer** — no upload to someone else's cloud that lingers, gets indexed, or expires behind a paywall.

## Install

From source (needs a recent stable [Rust](https://rustup.rs), 1.91+):

```sh
# clone + build
git clone https://github.com/StephenSHorton/hato
cd hato
cargo build --release
# the binary is target/release/hato

# …or install it onto your PATH directly from GitHub
cargo install --git https://github.com/StephenSHorton/hato hato-cli
```

## Usage

```sh
hato send <PATH>              # send a file or folder; prints a ticket
hato send --relay <PATH>      # force the transfer through iroh's relay (see below)
hato receive <TICKET> [DIR]   # download into DIR (default: current directory)
```

- **`send`** imports the path (referencing it in place — it does not copy your file) and serves it until you press **Ctrl+C**. Keep the window open until your friend has it.
- **`receive`** connects with the ticket, downloads, and writes the file(s) into `DIR`. Re-running the same ticket resumes an interrupted download.
- **`--relay`** mints a *relay-only* ticket with no direct addresses, forcing the connection through iroh's relay servers. Handy for testing the cross-internet path, or when you already know direct connectivity won't work.

## How it works

```
 hato send                                            hato receive
 ─────────                                            ────────────
 file ──▶ import into on-disk store (BLAKE3 hashed)
           │
           ├─▶ BlobTicket  =  EndpointAddr + hash ───▶ (paste the ticket)
           │                                                │
           └─▶ serve over iroh  ◀───── QUIC, hole-punched ──┘
                                 ◀───── or relay fallback ───┘
                                        end-to-end encrypted
                                        verified-streaming (resumable)
```

1. `hato send` imports your file into a local [iroh-blobs](https://github.com/n0-computer/iroh-blobs) store, which content-addresses it with a BLAKE3 hash, and starts an iroh endpoint serving it.
2. The **ticket** is that endpoint's address plus the hash. iroh's address includes a relay URL and any directly-reachable IPs.
3. `hato receive` opens a QUIC connection to the sender — hole-punched directly when possible, or through a relay when not — then downloads the missing byte ranges. Because every range is BLAKE3-verified, a re-run only fetches what's still missing.
4. Encryption is intrinsic: each iroh endpoint's ed25519 key *is* its TLS identity, so the transfer is authenticated and end-to-end encrypted with no extra setup.

## Roadmap

- [x] **Phase 0** — spike: send/receive over iroh, byte-verified
- [x] **Phase 1** — CLI: real progress + ETA, folders, crash-safe resume, relay-only mode
- [ ] **Phase 2** — short human-readable codes (`brave-otter-lantern`) instead of long tickets, + QR
- [ ] **Phase 3** — Tauri desktop GUI: drag-drop, live progress, tray

Today you copy a ticket; **Phase 2** shrinks that to a short spoken-word code backed by a tiny rendezvous service.

## Security

The transport is as strong as iroh's: QUIC + TLS 1.3, each endpoint authenticated by its ed25519 key. **A ticket is a bearer credential** — anyone who has it can fetch the file until you stop serving (Ctrl+C), so share it over a channel you trust. The upcoming short-code system (Phase 2) is being designed to keep this property while making codes short *and* single-use.

## Development

```sh
cargo build            # build the workspace
cargo test             # run unit tests
cargo clippy -- -D warnings
cargo fmt --all
```

CI runs `fmt`, `clippy -D warnings`, and `test` on Linux and Windows for every push and PR.

## Workspace

- [`crates/hato-core`](crates/hato-core) — the transport + transfer library (iroh + iroh-blobs)
- [`crates/hato-cli`](crates/hato-cli) — the `hato` command-line app

## Acknowledgements

Stands on the shoulders of [n0-computer](https://n0.computer)'s [iroh](https://github.com/n0-computer/iroh) and [iroh-blobs](https://github.com/n0-computer/iroh-blobs), and takes direct inspiration from their [`sendme`](https://github.com/n0-computer/sendme) and from [magic-wormhole](https://github.com/magic-wormhole/magic-wormhole).

## License

[MIT](./LICENSE)
