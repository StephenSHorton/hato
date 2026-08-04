# hato — transfer engine for [suzuri](https://github.com/StephenSHorton/suzuri)

> **Product status:** the standalone Hato app (Tauri GUI + Windows installers) is **discontinued**.  
> **Use [suzuri](https://github.com/StephenSHorton/suzuri)** for the terminal host, palette UI, and shipped downloads.  
> This repo is the **Rust/iroh transfer engine** (`hato` / `suzuri-transfer` CLI) that suzuri shells out to.

[![CI](https://github.com/StephenSHorton/hato/actions/workflows/ci.yml/badge.svg)](https://github.com/StephenSHorton/hato/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Built on iroh](https://img.shields.io/badge/built%20on-iroh-8A2BE2.svg)](https://github.com/n0-computer/iroh)

Peer-to-peer file transfer over [iroh](https://github.com/n0-computer/iroh): QUIC, NAT hole-punching, free relay fallback, E2E encryption, BLAKE3-verified resume.

## Where to download

| Want | Go here |
|------|---------|
| Terminal + transfer UI (recommended) | **[suzuri releases](https://github.com/StephenSHorton/suzuri/releases/latest)** |
| Engine CLI only (power users) | Build from this repo (below) |

Historical Hato v0.1 / v0.2 Windows installers remain on [old Releases](https://github.com/StephenSHorton/hato/releases) but are **not maintained**.

## Build the engine

Needs Rust 1.91+.

```sh
git clone https://github.com/StephenSHorton/hato
cd hato
cargo build --release -p hato-cli
# binaries: target/release/hato  and  target/release/suzuri-transfer  (same code)

# or
cargo install --git https://github.com/StephenSHorton/hato hato-cli
```

### Machine mode (for suzuri / hosts)

```sh
hato --json send ./file.bin          # NDJSON on stdout; keep alive until Ctrl+C
hato --json receive "$TICKET" ~/Downloads
```

Protocol: [`docs/machine-mode.md`](docs/machine-mode.md).

### Human CLI (still works)

```sh
hato send ./file.bin
hato receive blob… [dir]
hato me
# Short codes / pair need a mailbox (see docs/phase2-shortcodes.md)
```

Config dir: platform project dir for app `hato`, or override with `HATO_CONFIG_DIR`.  
suzuri sets this to `…/suzuri/transfer/`.

## Crates

| Crate | Role |
|-------|------|
| `hato-core` | send/receive, identity, contacts, offers |
| `hato-cli` | `hato` + `suzuri-transfer` binaries |
| `hato-code` | SPAKE2 short codes / pair |
| `hato-mailbox` | WebSocket rendezvous (dev / self-host) |
| `crates/hato-gui` | **retired** Tauri app (excluded from workspace; not released) |

## Security

Transport is iroh’s QUIC + TLS 1.3 (endpoint ed25519 = identity). A **ticket is a bearer credential** — anyone with it can fetch until you stop serving.

## License

MIT — see [LICENSE](./LICENSE).
