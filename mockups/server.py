#!/usr/bin/env python3
"""Mockups server: static files + a keyed Vexcel proxy. Usage: python3 mockups/server.py [port]"""
import hashlib
import json
import sys
import urllib.error
import urllib.request
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CACHE = ROOT / ".data" / "vexcel-cache"  # gitignored
UPSTREAM = "https://api.vexcelgroup.com/v2/"
ALLOWED = {"3d/tileset", "3d/tile", "ortho/tile"}


def load_env(path=ROOT / ".env"):
    env = {}
    for line in path.read_text().splitlines() if path.exists() else []:
        k, sep, v = line.strip().partition("=")
        if sep and not k.startswith("#"):
            env[k.strip()] = v.strip().strip("'\"")
    return env


ENV = load_env()


class Handler(SimpleHTTPRequestHandler):
    def do_GET(self):
        path, _, query = self.path.partition("?")
        if path == "/config.json":
            return self.send(200, "application/json", json.dumps({"matterportSdkKey": ENV.get("MATTERPORT_SDK_KEY", "")}).encode())
        if path.startswith("/vexcel/"):
            return self.proxy(path[len("/vexcel/"):], query)
        super().do_GET()

    def proxy(self, sub, query):
        if sub not in ALLOWED:
            return self.send(403, "text/plain", b"not allowed")
        key = ENV.get("VEXCEL_API_KEY")
        if not key:
            return self.send(503, "text/plain", b"VEXCEL_API_KEY missing")
        url = UPSTREAM + sub + ("?" + query if query else "")
        f = CACHE / hashlib.sha256(url.encode()).hexdigest()
        if f.exists():  # ponytail: cache never expires; delete .data/vexcel-cache to refresh
            ctype, _, body = f.read_bytes().partition(b"\n")
            return self.send(200, ctype.decode(), body, cached=True)
        req = urllib.request.Request(url, headers={"X-API-KEY": key})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                status, ctype, body = r.status, r.headers.get("Content-Type", "application/octet-stream"), r.read()
        except urllib.error.HTTPError as e:
            status, ctype, body = e.code, e.headers.get("Content-Type", "text/plain"), e.read()
        except (urllib.error.URLError, TimeoutError) as e:
            return self.send(502, "text/plain", str(e).encode())
        if status == 200:
            CACHE.mkdir(parents=True, exist_ok=True)
            f.write_bytes(ctype.encode() + b"\n" + body)
        self.send(status, ctype, body)

    def send(self, status, ctype, body, cached=False):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store" if self.path == "/config.json" else "max-age=86400")
        if cached:
            self.send_header("X-Cache", "hit")
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 4173
    srv = ThreadingHTTPServer(("127.0.0.1", port), partial(Handler, directory=str(HERE)))
    print(f"serving {HERE} on http://127.0.0.1:{port}", flush=True)
    srv.serve_forever()
