#!/bin/sh
set -eu

umask 077
config_dir=${XDG_CONFIG_HOME:?XDG_CONFIG_HOME is required}/subhub
mkdir -p "$config_dir"

python3 - "$config_dir" <<'PY'
import json
import pathlib
import sys

config_dir = pathlib.Path(sys.argv[1])
entry = {
    "provider": "codex",
    "credential": {
        "tokens": {
            "access_token": "smoke-provider-token",
            "refresh_token": "unused",
            "account_id": "smoke-account",
        }
    },
    "oauthAccount": None,
}
(config_dir / "index.json").write_text(json.dumps({
    "version": 1,
    "active": "smoke",
    "active_codex": "smoke",
    "credentials": ["smoke"],
}))
(config_dir / "credentials.json").write_text(json.dumps({
    "subhub-credentials": {"smoke": json.dumps(entry)}
}))
PY
chmod 0600 "$config_dir/index.json" "$config_dir/credentials.json"

openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout /smoke/iron-ca.key \
    -out /smoke/iron-ca.crt \
    -days 1 \
    -subj "/CN=Subhub Iron smoke CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign" \
    >/dev/null 2>&1

openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout /smoke/upstream.key \
    -out /smoke/upstream.crt \
    -days 1 \
    -subj "/CN=chatgpt.com" \
    -addext "subjectAltName=DNS:chatgpt.com" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,digitalSignature,keyEncipherment,keyCertSign" \
    -addext "extendedKeyUsage=serverAuth" \
    >/dev/null 2>&1

subhub gateway iron-config > /smoke/subhub-iron-fragment.yaml
{
    printf 'dns:\n  enabled: false\n'
    awk '
        { print }
        $0 == "proxy:" {
            print "  http_listen: \"127.0.0.1:18080\""
            print "  https_listen: \"127.0.0.1:18443\""
            print "  tunnel_listen: \"127.0.0.1:8080\""
            print "  upstream_deny_cidrs: []"
        }
    ' /smoke/subhub-iron-fragment.yaml
    printf '\ntls:\n'
    printf '  mode: "mitm"\n'
    printf '  ca_cert: "/smoke/iron-ca.crt"\n'
    printf '  ca_key: "/smoke/iron-ca.key"\n'
} > /smoke/iron.yaml
chmod 0600 /smoke/*.key /smoke/iron.yaml
