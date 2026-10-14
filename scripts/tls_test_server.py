#!/usr/bin/env python3
"""HTTP test server for the host harness (host/, scripts/host_tls_test.sh).

Serves /frame: a deterministic body with ETag + If-None-Match => 304, a
`wait=N` long-poll hold, a required `Authorization: Bearer` token, and
framing variants for the client's body handling.
"""
import argparse
import hashlib
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

MAX_HOLD_S = 3


def make_body(size: int) -> bytes:
    """SHA-256 blocks of "1:n", as host/src/main.rs recomputes them."""
    out = bytearray()
    n = 0
    while len(out) < size:
        out.extend(hashlib.sha256(f"1:{n}".encode()).digest())
        n += 1
    return bytes(out[:size])


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "tls-test-server/1"

    def do_GET(self):  # noqa: N802  (http.server API)
        url = urlparse(self.path)
        if url.path != "/frame":
            self.send_error(404)
            return
        if self.headers.get("Authorization") != f"Bearer {self.server.token}":
            self.send_error(403)
            return
        q = parse_qs(url.query)

        # Framing variants: len=N (a shorter body), short=1 (declare the
        # full length, send half, close), chunked=1, nolen=1 (no
        # Content-Length; the close ends it).
        size = int(q.get("len", [self.server.body_size])[0])
        body = make_body(size)
        etag = '"' + hashlib.sha256(body).hexdigest()[:32] + '"'

        # Long-poll: hold while the client's ETag still matches.
        wait_s = min(int(q.get("wait", ["0"])[0] or 0), MAX_HOLD_S)
        client_etag = self.headers.get("If-None-Match")
        if wait_s and client_etag == etag:
            time.sleep(wait_s)

        if client_etag == etag:
            self.send_response(304)
            self.send_header("ETag", etag)
            self.end_headers()
            return

        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("ETag", etag)
        if q.get("chunked") == ["1"]:
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.write(b"%x\r\n" % len(body) + body + b"\r\n0\r\n\r\n")
            return
        if q.get("nolen") == ["1"]:
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True
            return
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if q.get("short") == ["1"]:
            self.wfile.write(body[: len(body) // 2])
            self.close_connection = True
            return
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print(f"[server:{self.server.server_port}] {fmt % args}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--size", type=int, required=True, help="body bytes")
    ap.add_argument("--token", required=True, help="required bearer token")
    args = ap.parse_args()

    httpd = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    httpd.body_size = args.size
    httpd.token = args.token

    print(f"[server:{args.port}] http, {args.size}B body", file=sys.stderr)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
