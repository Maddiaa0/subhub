# sub-manager

A small macOS CLI for keeping multiple Claude Code OAuth credentials in the
login Keychain and switching the credential used by Claude Code.

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
sub-manager help
```

`add` runs `claude auth login`, captures the resulting
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
