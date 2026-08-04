# Machine mode (`--json`)

Host embedding protocol for **hato** / **suzuri-transfer**. When enabled, **stdout is NDJSON only** (one JSON object per line). Human text and tracing go to stderr.

## Enabling

```sh
hato --json <command> …
# or
HATO_OUTPUT=json hato <command> …
```

The same binary is also built as `suzuri-transfer` (identical entrypoint).

## Event envelope

Every line:

```json
{"v":1,"event":"<name>", …}
```

`v` is the protocol version (currently `1`). Hosts should ignore unknown fields and unknown events.

## Commands (v1)

| Command | Events (typical order) |
|--------|-------------------------|
| `send <path>` | `ready` → (serve until SIGINT) → `stopped` |
| `receive <ticket> [dir]` | `receiving` → `progress`* → `resumed`? → `done` |
| `me` | `me` |
| `contacts list` | `contacts` |
| `code` / `get` / `pair` / `listen` / `send --to` | see source; same style |

### `ready` (send)

```json
{"v":1,"event":"ready","ticket":"blob…","relays":1,"ips":2,"relay_only":false,"path":"/…"}
```

Keep the process alive until the peer finishes downloading, then SIGINT.

### `progress` / `done` (receive)

```json
{"v":1,"event":"progress","done":123,"total":456}
{"v":1,"event":"done","total_bytes":456,"already_had":0,"out_dir":"/…"}
```

Progress is throttled (~100 ms). `done` may briefly exceed `total` depending on iroh accounting — treat as informational.

### `error`

```json
{"v":1,"event":"error","code":"usage|generic|verifier_mismatch|…","message":"…"}
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Generic failure |
| 2 | Usage / missing path |
| 3 | Verifier mismatch (wrong short code) |
| 4 | Peer / contact failure |
| 5 | Offer rejected |
| 130 | Interrupted (normal end for `send` after Ctrl+C) |

## Config isolation

Set `HATO_CONFIG_DIR` so identity/contacts live under the host app (suzuri uses `<config>/transfer/`). Sender and receiver on the same machine **must** use different config dirs (distinct iroh identities); connecting to yourself is rejected.

## Env

| Variable | Role |
|----------|------|
| `HATO_CONFIG_DIR` | Config root (`identity.secret`, `contacts.json`, …) |
| `HATO_MAILBOX` | WebSocket mailbox URL for codes/pair |
| `HATO_OUTPUT=json` | Same as `--json` |
