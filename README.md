# Subhub

Keep Claude Code, Codex, and Antigravity CLI running across multiple subscriptions.

Subhub stores OAuth credentials securely for the current platform, reports subscription
usage, routes Claude Code and Codex through an account for the appropriate provider
that still has capacity, and switches Antigravity CLI accounts directly on disk.

## Features

- Multiple named Claude Code, Codex, and Antigravity CLI accounts.
- Usage visibility and capacity-aware routing for Claude Code and Codex.
- Session affinity with one safe retry after pre-stream `401` or `429` errors.
- Atomic account-file switching for Antigravity CLI (`~/.gemini/antigravity-cli/antigravity-oauth-token`) with automatic sync-back of tokens refreshed by `agy`.
- A persistent per-user macOS LaunchAgent or Linux systemd service.
- Keychain-backed storage on macOS and permission-restricted file storage on Linux.

## Getting started

Subhub requires macOS or Linux, Rust nightly 1.98 (`nightly-2026-06-21`) or a newer
nightly, and the `claude`, `codex`, and `agy` CLIs for their respective providers.

```sh
cargo install subhub

subhub add personal
subhub add work
subhub audit
subhub gateway install
```

To install from a local checkout, run `cargo install --path .`.

`subhub add` prompts for Claude Code, Codex, or Antigravity CLI (or pass `--provider claude|codex|antigravity` to skip the prompt). Claude accounts use the standard
`claude auth login --claudeai` flow by default. Pass `--capture` to import
Claude Code's or Antigravity CLI's current OAuth login without opening a browser. Codex accounts are
captured after a standard `codex login` ChatGPT flow in an isolated temporary
credential cache; pass `--device-auth` on remote or headless machines to use
the device-code flow instead of a local browser. Antigravity CLI accounts are
captured either with `--capture` (from `~/.gemini/antigravity-cli/antigravity-oauth-token`,
overridable via `SUBHUB_ANTIGRAVITY_TOKEN_PATH`) or via `agy acp --login` in an
isolated temporary home directory. On macOS, tokens are stored in the login Keychain. On Linux,
they are stored in `$XDG_CONFIG_HOME/subhub/credentials.json` (defaulting to
`~/.config/subhub/credentials.json`) with user-only permissions. Tokens are
never written to the Subhub index.

The Linux store is not encrypted at rest. Its security comes from the user's
filesystem permissions, so the config directory and credential file must not be
shared or made group/world-readable.

## Commands

```text
subhub add <name> [--provider claude|codex|antigravity] [--force] [--capture] [--device-auth]
                                  Save or replace an account
                                  (--capture: import Claude's or Antigravity's current login)
                                  (--device-auth: Codex device-code login)
subhub list                       List accounts and their providers
subhub set <name> [--provider claude|codex|antigravity]
                                  Select the preferred account (or swap the active
                                  Antigravity token file on disk)
subhub audit [--json]             Show subscription usage (and local Antigravity token status)
subhub push <name> --remote <url> Push a saved Claude/Codex credential to a remote
                                  gateway's admin API (token via
                                  --admin-token or SUBHUB_ADMIN_TOKEN)

subhub gateway install            Install and start the background gateway
subhub gateway reinstall          Uninstall then install, preserving accounts
subhub gateway status [--provider claude|codex|antigravity]
                                  Show gateway health, optionally filtered
subhub gateway logs [--lines 100] Show structured gateway events
subhub gateway doctor             Diagnose routing and credential health
subhub gateway start|stop|restart Manage the gateway
subhub gateway uninstall [--purge]
subhub gateway serve              Run the gateway in the foreground
subhub gateway auth-token         Print the local gateway token
```

## How it works

The authenticated gateway listens on `127.0.0.1:7842` by default. Serving
beyond the host (for example in a container or Kubernetes cluster) requires
`--allow-remote` together with an explicit client token; see `deploy/README.md`
for a container image and manifest.

A gateway started with `--admin-token` (or `SUBHUB_ADMIN_TOKEN`) exposes a
credential-push admin API: `subhub push <name> --remote <url>` uploads a
locally saved account to it, and the remote validates, persists, and reloads
it immediately. The admin token must differ from the client token — clients
holding the proxy token cannot write credentials — and a gateway with an
admin token may start with no saved credentials and wait to be seeded. It audits account
capacity every two minutes and keeps using the selected credential while it is
available. A pre-stream `401` or `429` may be retried once with another eligible
credential; interrupted streams are never replayed.

Claude Code requests are forwarded to Anthropic using saved Claude accounts.
Codex Responses API requests arrive under `/openai` and are forwarded to the
Codex upstream using saved Codex accounts. Provider selection is isolated, so
credentials are never routed across subscription types.

Antigravity CLI (`agy`) accounts are managed as a local account-file switcher
rather than proxied through the gateway: `subhub add <name> --provider antigravity`
saves the `antigravity-oauth-token` JSON in the vault, and `subhub set <name>`
atomically swaps `~/.gemini/antigravity-cli/antigravity-oauth-token` (syncing
any token refreshed by `agy` back to the previously active vault entry first).
`agy` continues talking directly to Google and handling its own OAuth refreshes.

The foreground server prints the environment needed by Claude Code:

```sh
subhub gateway serve
export ANTHROPIC_BASE_URL=http://127.0.0.1:7842
export ANTHROPIC_AUTH_TOKEN=<the-local-token-printed-by-serve>
claude
```

The local token authenticates clients to Subhub and is never forwarded
upstream. The gateway does not log request or response contents.

## Background integration

`subhub gateway install` creates and starts a per-user macOS LaunchAgent or
Linux systemd user service. It configures Claude Code with the local Anthropic base
URL and a secure-storage-backed authentication helper. It also adds a status-line
segment showing the routed Claude account and cached usage.

For Codex, installation adds a `subhub` Responses API provider to
`~/.codex/config.toml`, points `model_provider` at it, and configures the same
local authentication helper. Uninstall restores the prior Claude and Codex
settings when their current values are still managed by Subhub.

If the service needs to be rebuilt, run:

```sh
subhub gateway reinstall
```

This preserves saved subscriptions. To remove the integration and all Subhub
credentials, its gateway token, and its local index, run:

```sh
subhub gateway uninstall --purge
```

The gateway is the sole owner of each Claude refresh-token family. After
`subhub add` captures a login, it removes Claude Code's independent OAuth-token
copy; Claude Code authenticates to the local gateway instead. Duplicate Claude
account identities are rejected so aliases cannot race the same token family.
Uninstalling the gateway returns the selected credential to Claude Code after
the gateway has stopped. With `--purge`, Subhub then deletes its own copy; a
later `subhub add <name> --capture` imports Claude Code's current login without
requiring another login.

The gateway refreshes Claude OAuth credentials five minutes before access-token
expiry and retries one pre-stream request after refreshing on an upstream 401.
Refreshes are single-flighted per credential and protected by a cross-process
owner lock. Each refresh re-reads the vault after acquiring ownership, and a
rotated token is persisted before it is published to request handlers. OAuth
errors such as `invalid_grant` are recorded as terminal and are not retried,
including after a gateway restart; transport and provider failures use bounded
backoff. Re-add an account with `subhub add <name> --force` when its refresh
token has expired or been revoked. A running gateway is told to reload its
credentials after every `subhub add`, so new logins are routable immediately
without a restart.
