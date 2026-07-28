# sub-manager

A small macOS CLI for keeping multiple Claude Code OAuth credentials in the
login Keychain, auditing their subscription usage, and routing Claude Code
requests through a credential that has capacity available.

## Build and install

```sh
cargo install --path .
```

The program requires `claude`, `/usr/bin/security`, and macOS.

## Usage

```text
sub-manager add <name>
sub-manager add <name> --force
sub-manager list
sub-manager set <name>
sub-manager audit
sub-manager audit --json
sub-manager serve
sub-manager help
```

`add` runs `claude auth login --claudeai` with the OAuth scopes needed for
inference, usage auditing, Claude Code sessions, and account MCP servers. It
validates that Claude granted those scopes, then captures the resulting
`Claude Code-credentials` Keychain item and Claude account identity metadata,
and saves them together as a named Keychain entry. The newly added credential
remains active. A duplicate name fails unless `--force` is used.

`set` copies the selected named credential back to the
`Claude Code-credentials` Keychain item for the current `$USER` and restores
the corresponding `oauthAccount` metadata in `~/.claude.json`.

Credentials and their account metadata are stored as separate generic-password
items under the Keychain service `sub-manager-credentials`. Tokens and account
details are never written to the index.
The index contains only names and the active name at:

1. `$XDG_CONFIG/.sub-manager/index.json`, if `XDG_CONFIG` is set
2. `$XDG_CONFIG_HOME/.sub-manager/index.json`, otherwise
3. `$HOME/.config/.sub-manager/index.json`, otherwise

## Usage audit

`audit` queries Anthropic's Claude OAuth usage endpoint for each saved
credential. It reports the rolling five-hour and seven-day utilization and
reset times. The saved OAuth token must include the `user:profile` scope.

```sh
sub-manager audit
```

This endpoint is currently a beta interface used by Claude tooling. An
unauthorized or incompatible credential is reported as unavailable; refresh it
with `sub-manager add <name> --force`.

## Local credential router

`serve` starts an authenticated streaming proxy on
`127.0.0.1:7842`. At startup and every two minutes it audits all credentials.
It keeps using the selected credential while it remains below the usage
threshold, preserving session and prompt-cache affinity. If an upstream request
is rejected with `401` or `429` before streaming begins, it can retry once with
another eligible credential.

```sh
sub-manager serve
```

The command prints values to use in the shell where Claude Code will run:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:7842
export ANTHROPIC_AUTH_TOKEN=<the local token printed by serve>
claude
```

The local token authenticates Claude Code to the proxy. It is never forwarded
to Anthropic; the proxy replaces it with the selected saved OAuth token. To use
a stable local token, set `SUB_MANAGER_CLIENT_TOKEN` before starting the
server.

The server intentionally refuses non-loopback addresses, does not log request
or response contents, and defaults to retaining one percent of each applicable
usage window:

```sh
sub-manager serve --reserve-percent 1 --audit-interval 120
```

With the proxy running, its sanitized state is available at:

```sh
curl -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  "$ANTHROPIC_BASE_URL/_sub-manager/status"
```

Limitations of this MVP:

- OAuth refresh remains delegated to Claude Code. Re-add an expired credential.
- Request bodies are bounded at 32 MiB and buffered before forwarding.
- Automatic replay occurs only after a pre-stream `401` or `429`; interrupted
  streaming responses are never replayed.
- Non-message Anthropic endpoints are passed through with the same selected
  credential but have not all been individually characterized.
