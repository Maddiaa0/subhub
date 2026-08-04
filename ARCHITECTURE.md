# Architecture

Subhub is a CLI plus a local gateway that routes Claude Code and Codex
traffic through saved subscription credentials, picking the account with the
most headroom and rotating when one hits its limits.

## The one flow that explains everything

```
subhub add ──► vault (Keychain / 0600 store)     credentials/vault.rs
                 │  names + active slots ──►     credentials/index.rs
                 ▼
gateway serve ─► StoredCredential snapshot       credentials/mod.rs
                 │
                 ├─ audit loop: fetch usage per credential, record
                 │  CredentialHealth (advisory on transient failure)          gateway/audit.rs
                 ├─ refresh loop: refresh Claude tokens due within 60s,
                 │  exponential backoff per credential                        gateway/refresh.rs
                 ▼
request ───────► select_credential: sticky choice → least-utilized
                 eligible → transient-audit fallback                          gateway/selection.rs
                 │
                 ▼
forward ───────► provider upstream (path rewrite, beta headers),
                 401/429 → mark failed, retry with another credential         gateway/routes.rs
```

The CLI's `gateway status` / `doctor` / `statusline` read the gateway's
`/_subhub/status` endpoint; its wire format is the typed
`gateway/protocol.rs`, shared by server and consumers.

## Module map

| Module | Responsibility |
|---|---|
| `cli` | clap types, dispatch, `add`/`list`/`set`/`audit` handlers |
| `error` | typed errors; routing decisions use error *kinds*, never message text |
| `provider` | the per-provider seam: everything Claude/Codex-specific in one impl |
| `paths` | config file locations, atomic owner-only JSON writes |
| `credentials/vault` | OS secret storage; only this module touches raw vault payloads |
| `credentials/index` | credential names + active slots; never stores secrets |
| `credentials/oauth` | Claude OAuth refresh protocol, scopes, oauthAccount I/O |
| `gateway` | the loopback proxy: state, routes, selection, refresh, audit, protocol |
| `service` | install/uninstall, LaunchAgent/systemd management, admin-endpoint client |
| `output` | terminal rendering (health listings, status-line segment) |
| `observability` | append-only JSONL event log for `gateway logs` |
| `usage`, `codex` | provider usage snapshots and endpoints |

## Invariants

- **Secrets stay in the vault.** The index, logs, generated helpers, and
  service files never contain tokens (test-enforced).
- **Loopback only.** The gateway refuses non-loopback listen addresses; admin
  endpoints require the local bearer token.
- **Audits are advisory.** A transient audit failure must not make a working
  credential unroutable; fatal audit errors must (see `ErrorKind`).
- **Install is surgical.** `service` records exactly what it changed in
  Claude/Codex settings and restores only that on uninstall.
- **Errors are typed.** Never match on error message text; add a variant or
  kind in `error.rs` instead.

## Extending

To add a provider: add a `Provider` variant and follow the compiler through
the exhaustive matches (`provider.rs` first, then the usage-fetch dispatch in
`gateway/audit.rs` and `cli.rs`, and the vault payload shape in
`credentials/mod.rs`).

## Testing

`cargo test` — unit tests live next to their subjects; protocol-level tests
(OAuth refresh wire format, status endpoint shape) run against local axum
servers or serde round-trips. Live testing on a dev machine:
`cargo install --path . && subhub gateway restart`, then `subhub gateway status`.
