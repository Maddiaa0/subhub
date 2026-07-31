# Subhub

Keep Claude Code and Codex running across multiple subscriptions.

Subhub stores OAuth credentials in the macOS Keychain, reports subscription
usage, and routes both CLIs through an account for the appropriate provider
that still has capacity.

## Features

- Multiple named Claude Code and Codex accounts.
- Usage visibility and capacity-aware routing.
- Session affinity with one safe retry after pre-stream `401` or `429` errors.
- A persistent, per-user macOS LaunchAgent.
- Keychain-backed credential and local gateway-token storage.

## Getting started

Subhub requires macOS, Rust 1.85 or newer, and the `claude` and `codex` CLIs
for their respective providers.

```sh
cargo install subhub

subhub add personal
subhub add work
subhub audit
subhub gateway install
```

To install from a local checkout, run `cargo install --path .`.

`subhub add` prompts for Claude Code or Codex. Claude accounts are captured
after the standard `claude auth login --claudeai` flow. Codex accounts are
captured after a standard `codex login` ChatGPT flow in an isolated temporary
credential cache. Tokens are stored in the macOS login Keychain and are never
written to the Subhub index.

## Commands

```text
subhub add <name> [--force]       Save or replace an account
subhub list                       List accounts and their providers
subhub set <name>                 Select an account
subhub audit [--json]             Show subscription usage

subhub gateway install            Install and start the background gateway
subhub gateway reinstall          Uninstall then install, preserving accounts
subhub gateway status             Show gateway health
subhub gateway start|stop|restart Manage the gateway
subhub gateway uninstall [--purge]
subhub gateway serve              Run the gateway in the foreground
subhub gateway auth-token         Print the local gateway token
```

## How it works

The authenticated gateway listens only on `127.0.0.1:7842`. It audits account
capacity every two minutes and keeps using the selected credential while it is
available. A pre-stream `401` or `429` may be retried once with another eligible
credential; interrupted streams are never replayed.

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

## Background integration

`subhub gateway install` creates and starts the per-user LaunchAgent
`com.subhub.gateway`. It configures Claude Code with the local Anthropic base
URL and a Keychain-backed authentication helper. It also adds a status-line
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

OAuth refresh remains delegated to the provider CLIs. Re-add an expired account
with `subhub add <name> --force`.
