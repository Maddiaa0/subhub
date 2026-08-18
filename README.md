# Subhub

Keep Claude Code and Codex running across multiple subscriptions.

Subhub stores OAuth credentials securely for the current platform, reports subscription
usage, and routes both CLIs through an account for the appropriate provider
that still has capacity.

## Features

- Multiple named Claude Code and Codex accounts.
- Usage visibility and capacity-aware routing.
- Session affinity with one safe retry after pre-stream `401` or `429` errors.
- Optional Iron Proxy integration that keeps Iron as the egress data plane and
  uses Subhub as the dynamic credential-pool control plane.
- A persistent per-user macOS LaunchAgent or Linux systemd service.
- Keychain-backed storage on macOS and permission-restricted file storage on Linux.

## Getting started

Subhub requires macOS or Linux, Rust nightly 1.98 (`nightly-2026-06-21`) or a newer
nightly, and the `claude` and `codex` CLIs for their respective providers.

```sh
cargo install subhub

subhub add personal
subhub add work
subhub audit
subhub gateway install
```

To install from a local checkout, run `cargo install --path .`.

`subhub add` prompts for Claude Code or Codex. Claude accounts use the standard
`claude auth login --claudeai` flow by default. Pass `--capture` to import
Claude Code's current OAuth login without opening a browser. Codex accounts are
captured after a standard `codex login` ChatGPT flow in an isolated temporary
credential cache; pass `--device-auth` on remote or headless machines to use
the device-code flow instead of a local browser. On macOS, tokens are stored in the login Keychain. On Linux,
they are stored in `$XDG_CONFIG_HOME/subhub/credentials.json` (defaulting to
`~/.config/subhub/credentials.json`) with user-only permissions. Tokens are
never written to the Subhub index.

The Linux store is not encrypted at rest. Its security comes from the user's
filesystem permissions, so the config directory and credential file must not be
shared or made group/world-readable.

## Commands

```text
subhub add <name> [--force] [--capture] [--device-auth]
                                  Save or replace an account
                                  (--capture: import Claude's current login)
                                  (--device-auth: Codex device-code login)
subhub list                       List accounts and their providers
subhub set <name> [--provider claude|codex]
                                  Select the gateway's preferred account
subhub audit [--json]             Show subscription usage

subhub gateway install [--transport direct|iron]
                                  Install and start the background gateway
subhub gateway reinstall [--transport direct|iron]
                                  Uninstall then install, preserving accounts
subhub gateway status [--provider claude|codex]
                                  Show gateway health, optionally filtered
subhub gateway logs [--lines 100] Show structured gateway events
subhub gateway doctor             Diagnose routing and credential health
subhub gateway start|stop|restart Manage the gateway
subhub gateway uninstall [--purge]
subhub gateway serve [--transport direct|iron]
                                  Run the gateway in the foreground
subhub gateway auth-token         Print the local gateway token
subhub gateway iron-config [--listen ADDR] [--iron-grpc-listen ADDR]
                             [--iron-sandbox-id ID]
                                  Print the pinned Iron configuration fragment
subhub gateway iron-token         Print the dedicated retry callback token
```

## How it works

The direct gateway (the default) listens only on `127.0.0.1:7842`. It audits
account capacity every two minutes and keeps using the selected credential
while it is available. A pre-stream `401` or `429` may be retried once;
interrupted streams are never replayed.

Claude Code requests are forwarded to Anthropic using saved Claude accounts.
Codex Responses API requests arrive under `/openai` and are forwarded to the
Codex upstream using saved Codex accounts. Provider selection is isolated, so
credentials are never routed across subscription types.

The foreground server prints the environment needed by Claude Code:

```sh
subhub gateway serve
export ANTHROPIC_BASE_URL=http://127.0.0.1:7842
export ANTHROPIC_AUTH_TOKEN=<the-local-token-printed-by-serve>
claude
```

The local token authenticates clients to Subhub and is never forwarded
upstream. The gateway does not log request or response contents.

## Iron Proxy mode

Iron mode keeps [Iron Proxy](https://github.com/ironsh/iron-proxy) as the only
request data plane. Claude Code and Codex use their official provider URLs and
receive only an inert placeholder credential. Iron terminates the sandbox TLS
connection and asks Subhub which real credential headers to attach through
Iron's external gRPC transform API:

```text
Claude Code / Codex
        │ official provider URL + inert placeholder
        ▼
Iron Proxy ── gRPC TransformRequest ──► Subhub credential pool
        │                                  │ select account A
        │◄──────── modified headers ───────┘
        ▼
Anthropic / Codex
        │ replayable 429
        ▼
Iron Proxy ── retry callback ─────────► Subhub selects account B
        └──────── one exact replay ───► provider
```

The integration is pinned to Iron `v0.50.0-rc.3`. Start Subhub on the same host
or network namespace as Iron, merge the generated fragment into Iron's main
YAML configuration, and export the five commented retry-handler variables
shown at its end:

```sh
subhub gateway serve --transport iron
subhub gateway iron-config > subhub-iron-fragment.yaml
```

The default local layout is:

```text
127.0.0.1:7842  Subhub admin and authenticated Iron retry callbacks
127.0.0.1:7843  Subhub TransformService (plaintext gRPC)
```

Only `POST https://api.anthropic.com/v1/messages` and
`POST https://chatgpt.com/backend-api/codex/responses` can receive credentials.
Subhub replaces client-supplied `authorization` and `x-api-key` headers and
preserves other client headers. It does not enable Iron's optional
`header_allowlist`; add one only after observing every header required by the
specific Claude Code and Codex versions in use.

Subhub accepts inference request bodies up to 32 MiB in either transport.
The generated Iron configuration buffers one additional byte so oversized
requests are rejected with `413` instead of forwarding a truncated prefix.
When using custom listener addresses or a custom sandbox ID, pass the same
values to both `gateway serve` and `gateway iron-config`.

The sandbox must trust Iron's MITM CA. Iron validates the providers' public
certificates normally. Iron-to-Subhub traffic is plaintext loopback, so this
local mode introduces no second CA. If Iron runs in a separate container,
`127.0.0.1` refers to that container; use a shared network namespace (for
example host networking on Linux) rather than exposing Subhub's control ports.

Iron may ask Subhub for one exact replay when the provider returns `401` or
`429` and the complete request body fits Iron's configured buffer. A `429`
marks account A exhausted and retries with eligible account B. A Claude `401`
force-refreshes A and retries with the same identity; Codex `401` responses are
not switched mid-request. Unknown destinations, mismatched correlation,
non-replayable bodies, and second retry attempts are refused. SSE responses
are not migrated after streaming begins.

## Background integration

`subhub gateway install` creates and starts a per-user macOS LaunchAgent or
Linux systemd user service. Direct mode configures Claude Code with the local
Anthropic base URL. `--transport iron` retains the official Anthropic URL.
Both modes use a secure-storage-backed authentication helper and add a
status-line segment showing the routed Claude account and cached usage.

For Codex, installation adds a `subhub` Responses API provider to
`~/.codex/config.toml`, points `model_provider` at it, and configures the same
local authentication helper. Iron mode uses the official ChatGPT Codex base
URL and makes both helpers return only the inert placeholder. Uninstall restores
the prior Claude and Codex settings when their current values are still managed
by Subhub.

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
