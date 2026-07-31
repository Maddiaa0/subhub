# Subhub

<h4 align="center">
  Keep Claude Code running across multiple subscriptions.
</h4>

<p align="center">
  Store OAuth credentials in the macOS Keychain, track usage, and route Claude Code to an account with capacity.
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#getting-started">Getting Started</a> •
  <a href="#commands">Commands</a> •
  <a href="#how-it-works">How It Works</a>
</p>

## Features

- **Multiple accounts** — save Claude Code subscriptions under friendly names.
- **Usage visibility** — view current utilization and reset times across every account.
- **Automatic routing** — keep session affinity while an account has capacity, then switch when needed.
- **Background service** — install once and use Claude Code normally.
- **Keychain storage** — keep credentials and the gateway token out of config files and process arguments.

## Getting Started

Subhub requires macOS, Rust 1.85 or newer, and the `claude` CLI.

```sh
cargo install subhub

subhub add personal
subhub add work
subhub audit
subhub gateway install
```

To install from a local checkout instead, run `cargo install --path .`.

Installation starts a per-user LaunchAgent and configures Claude Code to use
the local gateway. It also adds a status-line segment with the active account
and its cached usage.

```text
Subhub: personal | 5h 12% | 7d 35%
```

## Commands

```text
subhub add <name> [--force]       Save or replace an account
subhub list                       List saved accounts
subhub set <name>                 Select an account
subhub audit [--json]             Show subscription usage

subhub gateway install            Install and start the background gateway
subhub gateway reinstall          Run uninstall then install in one command
subhub gateway status             Show gateway health
subhub gateway start|stop|restart Manage the gateway
subhub gateway uninstall [--purge]
```

Run `subhub help` or `subhub <command> --help` for all options.

## How It Works

Subhub stores OAuth credentials in the macOS login Keychain. Its authenticated
gateway listens only on `127.0.0.1:7842`, audits account capacity every two
minutes, and preserves the selected account while it remains available.
Pre-stream `401` and `429` responses may be retried once with another eligible
account; interrupted streams are never replayed.

The local gateway token is replaced with the selected OAuth token before a
request is forwarded. Subhub does not log request or response contents.

Use the foreground server for debugging:

```sh
subhub gateway serve
```

To remove the integration but keep saved accounts:

```sh
subhub gateway uninstall
```

If restarting does not recover the gateway, run:

```sh
subhub gateway reinstall
```

This runs `subhub gateway uninstall` followed by `subhub gateway install` in a
single command. It does not purge saved account credentials.

Add `--purge` to also delete Subhub credentials, its gateway token, and its
local index. OAuth refresh is delegated to Claude Code; re-add an expired
account with `subhub add <name> --force`.
