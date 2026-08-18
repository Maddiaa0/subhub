#!/usr/bin/env python3
import json
import ssl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/backend-api/wham/usage":
            self.send_error(503, "intentional transient audit failure")
            return
        self.send_error(404)

    def do_POST(self):
        expected_headers = {
            "authorization": "Bearer smoke-provider-token",
            "chatgpt-account-id": "smoke-account",
            "openai-beta": "codex-1",
        }
        invalid = {
            name: self.headers.get(name)
            for name, value in expected_headers.items()
            if self.headers.get(name) != value
        }
        if self.path != "/backend-api/codex/responses" or invalid:
            self.send_error(401, f"credential headers were not replaced: {invalid}")
            return

        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            self.send_error(400, "request body was not valid JSON")
            return
        if payload.get("input") != "subhub-iron-smoke":
            self.send_error(400, "request body was not preserved")
            return

        response = json.dumps({"ok": True, "source": "subhub-iron-smoke"}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, format, *args):
        print(f"fake-provider: {format % args}", flush=True)


server = ThreadingHTTPServer(("127.0.0.1", 443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("/smoke/upstream.crt", "/smoke/upstream.key")
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
