# Hato 🐦

**A carrier pigeon for your files.** Fast, free, peer-to-peer file transfer — send a file straight to a friend over the internet, no cloud, no account, no size cap.

Built in Rust on [iroh](https://github.com/n0-computer/iroh) (QUIC + NAT hole-punching + free relay fallback + end-to-end encryption). Think *magic-wormhole*, with a native drag-and-drop app.

```sh
# sender
hato send big-file.zip
#  → hato flies out with code:  brave-otter-lantern

# receiver
hato receive brave-otter-lantern
```

The file streams **directly** between the two machines, end-to-end encrypted, and **resumes** if the connection drops (BLAKE3 verified streaming) — so multi-gigabyte sends are safe.

## Status

Early / work in progress. Roadmap:

- [ ] **Phase 0** — spike: send/receive over iroh across the internet, prove resume
- [ ] **Phase 1** — CLI MVP: progress + ETA, folders, resume
- [ ] **Phase 2** — short human-readable codes (rendezvous shortener) + QR
- [ ] **Phase 3** — Tauri desktop GUI: drag-drop, live progress, tray

## Workspace

- `crates/hato-core` — the transport + transfer library (iroh + iroh-blobs)
- `crates/hato-cli` — the `hato` command-line app

## License

MIT
